import { test, expect } from './fixtures';
import { finalizeEvent, getPublicKey, nip19 } from 'nostr-tools';
import WebSocket from 'ws';
import { createHash } from 'crypto';
import { setupPageErrorHandler, disableOthersPool, ensureLoggedIn, waitForRelayConnected, useLocalRelay } from './test-utils';
import { BOOTSTRAP_SECKEY_HEX, FOLLOW_SECKEY_HEX } from './nostr-test-keys';

let relayUrl = '';
test.beforeAll(({ relayUrl: workerRelayUrl }) => {
  relayUrl = workerRelayUrl;
});
const BOOTSTRAP_PUBKEY = getPublicKey(BOOTSTRAP_SECKEY_HEX);
const FOLLOW_PUBKEY = getPublicKey(FOLLOW_SECKEY_HEX);
const BOOTSTRAP_NPUB = nip19.npubEncode(BOOTSTRAP_PUBKEY);
const FOLLOW_NPUB = nip19.npubEncode(FOLLOW_PUBKEY);

async function publishEvent(event: Record<string, unknown>): Promise<void> {
  await new Promise<void>((resolve, reject) => {
    const socket = new WebSocket(relayUrl);
    const timeout = setTimeout(() => {
      socket.close();
      reject(new Error('Timed out publishing event'));
    }, 2000);

    socket.on('open', () => {
      socket.send(JSON.stringify(['EVENT', event]));
    });

    socket.on('message', (data) => {
      try {
        const msg = JSON.parse(data.toString());
        if (Array.isArray(msg) && msg[0] === 'OK' && msg[1] === (event as { id?: string }).id) {
          clearTimeout(timeout);
          socket.close();
          resolve();
        }
      } catch {
        // ignore parse errors
      }
    });

    socket.on('error', (err) => {
      clearTimeout(timeout);
      reject(err);
    });
  });
}

async function seedFeedVideos(suffix: string): Promise<{ bootstrapTree: string; followTree: string }> {
  const now = Math.floor(Date.now() / 1000);
  const bootstrapKey = Buffer.from(BOOTSTRAP_SECKEY_HEX, 'hex');
  const followKey = Buffer.from(FOLLOW_SECKEY_HEX, 'hex');

  const bootstrapTree = `videos/Feed Multi A ${suffix}`;
  const followTree = `videos/Feed Multi B ${suffix}`;
  const bootstrapVideoHash = createHash('sha256').update(`feed-multi-${suffix}-bootstrap`).digest('hex');
  const followVideoHash = createHash('sha256').update(`feed-multi-${suffix}-follow`).digest('hex');

  const followEvent = finalizeEvent({
    kind: 3,
    content: '',
    tags: [['p', FOLLOW_PUBKEY]],
    created_at: now,
    pubkey: BOOTSTRAP_PUBKEY,
  }, bootstrapKey);

  const bootstrapVideoEvent = finalizeEvent({
    kind: 30078,
    content: '',
    tags: [
      ['d', bootstrapTree],
      ['l', 'hashtree'],
      ['hash', bootstrapVideoHash],
    ],
    created_at: now,
    pubkey: BOOTSTRAP_PUBKEY,
  }, bootstrapKey);

  const followVideoEvent = finalizeEvent({
    kind: 30078,
    content: '',
    tags: [
      ['d', followTree],
      ['l', 'hashtree'],
      ['hash', followVideoHash],
    ],
    created_at: now,
    pubkey: FOLLOW_PUBKEY,
  }, followKey);

  await publishEvent(followEvent);
  await publishEvent(bootstrapVideoEvent);
  await publishEvent(followVideoEvent);

  return { bootstrapTree, followTree };
}

test('new user feed shows videos from multiple owners', async ({ page }) => {
  test.slow();
  setupPageErrorHandler(page);

  const suffix = Date.now().toString(36);
  await seedFeedVideos(suffix);

  await page.goto('/video.html#/');
  await disableOthersPool(page);
  await useLocalRelay(page);
  await ensureLoggedIn(page);
  await waitForRelayConnected(page, 30000);

  const refreshFeed = async () => {
    await page.evaluate(async () => {
      const { resetFeedFetchState, fetchFeedVideos } = await import('/src/stores/feedStore');
      resetFeedFetchState();
      await fetchFeedVideos();
    });
  };

  await refreshFeed();

  await expect.poll(
    async () => {
      const result = await page.evaluate(({ bootstrapNpub, followNpub }) => {
        const hasBootstrap = !!document.querySelector(`a[href*="${bootstrapNpub}"][href*="videos%2F"]`);
        const hasFollow = !!document.querySelector(`a[href*="${followNpub}"][href*="videos%2F"]`);
        return { hasBootstrap, hasFollow };
      }, { bootstrapNpub: BOOTSTRAP_NPUB, followNpub: FOLLOW_NPUB });

      if (!result.hasBootstrap || !result.hasFollow) {
        await refreshFeed();
      }

      return result;
    },
    { timeout: 60000, intervals: [1000, 2000, 3000] }
  ).toEqual({ hasBootstrap: true, hasFollow: true });

  await expect(page.locator(`a[href*="${BOOTSTRAP_NPUB}"][href*="videos%2F"]`).first()).toBeVisible({ timeout: 20000 });
  await expect(page.locator(`a[href*="${FOLLOW_NPUB}"][href*="videos%2F"]`).first()).toBeVisible({ timeout: 20000 });
});
