import { test, expect } from './fixtures';
import { finalizeEvent, getPublicKey, nip19 } from 'nostr-tools';
import WebSocket from 'ws';
import { createHash } from 'crypto';
import { setupPageErrorHandler, disableOthersPool, ensureLoggedIn, waitForRelayConnected, useLocalRelay, presetLocalRelayInDB, safeReload, waitForAppReady, clearAllStorage, waitForFollowInWorker } from './test-utils';
import { BOOTSTRAP_SECKEY_HEX, FOLLOW_SECKEY_HEX, FOLLOW2_SECKEY_HEX } from './nostr-test-keys';

let relayUrl = '';
test.beforeAll(({ relayUrl: workerRelayUrl }) => {
  relayUrl = workerRelayUrl;
});
const BOOTSTRAP_PUBKEY = getPublicKey(BOOTSTRAP_SECKEY_HEX);
const FOLLOW_PUBKEY = getPublicKey(FOLLOW_SECKEY_HEX);
const FOLLOW_NPUB = nip19.npubEncode(FOLLOW_PUBKEY);
const FOLLOW2_PUBKEY = getPublicKey(FOLLOW2_SECKEY_HEX);
const FOLLOW2_NPUB = nip19.npubEncode(FOLLOW2_PUBKEY);

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

async function seedFeedVideos(suffix: string): Promise<{ followTree: string; follow2Tree: string }> {
  const now = Math.floor(Date.now() / 1000);
  const bootstrapKey = Buffer.from(BOOTSTRAP_SECKEY_HEX, 'hex');
  const followKey = Buffer.from(FOLLOW_SECKEY_HEX, 'hex');
  const follow2Key = Buffer.from(FOLLOW2_SECKEY_HEX, 'hex');

  const followTree = `videos/Feed Multi A ${suffix}`;
  const follow2Tree = `videos/Feed Multi B ${suffix}`;
  const followVideoHash = createHash('sha256').update(`feed-multi-${suffix}-follow`).digest('hex');
  const follow2VideoHash = createHash('sha256').update(`feed-multi-${suffix}-follow2`).digest('hex');

  const followEvent = finalizeEvent({
    kind: 3,
    content: '',
    tags: [['p', FOLLOW_PUBKEY], ['p', FOLLOW2_PUBKEY]],
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
    created_at: now + 2,
    pubkey: FOLLOW_PUBKEY,
  }, followKey);

  const follow2VideoEvent = finalizeEvent({
    kind: 30078,
    content: '',
    tags: [
      ['d', follow2Tree],
      ['l', 'hashtree'],
      ['hash', follow2VideoHash],
    ],
    created_at: now + 3,
    pubkey: FOLLOW2_PUBKEY,
  }, follow2Key);

  await publishEvent(followEvent);
  await publishEvent(followVideoEvent);
  await publishEvent(follow2VideoEvent);

  return { followTree, follow2Tree };
}

test('new user feed shows videos from multiple owners', async ({ page }) => {
  test.slow();
  setupPageErrorHandler(page);

  const suffix = Date.now().toString(36);
  const { followTree, follow2Tree } = await seedFeedVideos(suffix);

  await page.goto('/video.html#/');
  await waitForAppReady(page);
  await clearAllStorage(page);
  await presetLocalRelayInDB(page);
  await safeReload(page, { waitUntil: 'domcontentloaded', timeoutMs: 60000, url: 'http://localhost:5173/video.html#/' });
  await waitForAppReady(page);
  await disableOthersPool(page);
  await useLocalRelay(page);
  await ensureLoggedIn(page);
  await waitForRelayConnected(page, 30000);
  await page.waitForFunction(() => (window as any).__testHelpers?.followPubkey, { timeout: 10000 });
  await page.evaluate((pk: string) => (window as any).__testHelpers?.followPubkey?.(pk), FOLLOW_PUBKEY);
  await page.evaluate((pk: string) => (window as any).__testHelpers?.followPubkey?.(pk), FOLLOW2_PUBKEY);
  await waitForFollowInWorker(page, FOLLOW_PUBKEY, 20000);
  await waitForFollowInWorker(page, FOLLOW2_PUBKEY, 20000);

  const refreshFeed = async () => {
    await page.evaluate(async () => {
      (window as any).__workerAdapter?.sendHello?.();
      const { feedStore, resetFeedFetchState, fetchFeedVideos } = await import('/src/stores/feedStore');
      feedStore.set([]);
      resetFeedFetchState();
      await fetchFeedVideos();
    });
  };

  await refreshFeed();
  const followEncoded = encodeURIComponent(followTree);
  const follow2Encoded = encodeURIComponent(follow2Tree);

  await expect.poll(
    async () => {
      const result = await page.evaluate(({ followNpub, followPath, follow2Npub, follow2Path }) => {
        const hasFollow = !!document.querySelector(`a[href*="${followNpub}"][href*="${followPath}"]`);
        const hasFollow2 = !!document.querySelector(`a[href*="${follow2Npub}"][href*="${follow2Path}"]`);
        return { hasFollow, hasFollow2 };
      }, {
        followNpub: FOLLOW_NPUB,
        followPath: followEncoded,
        follow2Npub: FOLLOW2_NPUB,
        follow2Path: follow2Encoded,
      });

      if (!result.hasFollow || !result.hasFollow2) {
        await refreshFeed();
      }

      return result;
    },
    { timeout: 180000, intervals: [1000, 2000, 3000] }
  ).toEqual({ hasFollow: true, hasFollow2: true });

  await expect(page.locator(`a[href*="${FOLLOW_NPUB}"][href*="${followEncoded}"]`).first()).toBeVisible({ timeout: 20000 });
  await expect(page.locator(`a[href*="${FOLLOW2_NPUB}"][href*="${follow2Encoded}"]`).first()).toBeVisible({ timeout: 20000 });
});
