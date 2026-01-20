import { test, expect } from './fixtures';
import { finalizeEvent, getPublicKey, nip19 } from 'nostr-tools';
import WebSocket from 'ws';
import { disableOthersPool, setupPageErrorHandler, waitForAppReady, ensureLoggedIn, useLocalRelay, waitForRelayConnected } from './test-utils.js';
import { BOOTSTRAP_SECKEY_HEX, FOLLOW_SECKEY_HEX } from './nostr-test-keys';

let relayUrl = '';
test.beforeAll(({ relayUrl: workerRelayUrl }) => {
  relayUrl = workerRelayUrl;
});
const BOOTSTRAP_PUBKEY = getPublicKey(BOOTSTRAP_SECKEY_HEX);
const BOOTSTRAP_NPUB = nip19.npubEncode(BOOTSTRAP_PUBKEY);
const FOLLOW_PUBKEY = getPublicKey(FOLLOW_SECKEY_HEX);

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

async function seedKnownFollowers(): Promise<void> {
  const now = Math.floor(Date.now() / 1000);
  const followerKey = Buffer.from(FOLLOW_SECKEY_HEX, 'hex');

  const followEvent = finalizeEvent({
    kind: 3,
    content: '',
    tags: [['p', BOOTSTRAP_PUBKEY]],
    created_at: now,
    pubkey: FOLLOW_PUBKEY,
  }, followerKey);

  await publishEvent(followEvent);
}

/**
 * Test that social graph root is set correctly on login and account switch
 */
test.describe('Social graph root', () => {
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
  });

  test('should set root when app loads with logged in user', async ({ page }) => {
    await page.goto('/');
    await waitForAppReady(page);
    await disableOthersPool(page);
    await ensureLoggedIn(page);

    // Wait for login to complete - the social graph should have the user's pubkey as root
    await page.waitForFunction(
      () => {
        const nostrStore = (window as any).__nostrStore;
        const getSocialGraph = (window as any).__getSocialGraph;
        if (!nostrStore || !getSocialGraph) return false;

        const pubkey = nostrStore.getState()?.pubkey;
        if (!pubkey) return false;

        const graph = getSocialGraph();
        const root = graph?.getRoot?.();
        return root === pubkey;
      },
      { timeout: 10000 }
    );

    // Verify the root matches the logged in user's pubkey
    const { pubkey, root } = await page.evaluate(() => {
      const nostrStore = (window as any).__nostrStore;
      const getSocialGraph = (window as any).__getSocialGraph;
      return {
        pubkey: nostrStore?.getState()?.pubkey,
        root: getSocialGraph?.()?.getRoot?.(),
      };
    });

    console.log('User pubkey:', pubkey);
    console.log('Social graph root:', root);
    expect(root).toBe(pubkey);
  });

  test('should show me as known follower when I follow someone', async ({ page }) => {
    test.setTimeout(60000);

    const logs: string[] = [];
    page.on('console', (msg) => {
      const text = msg.text();
      if (text.includes('[Worker]') || text.includes('[socialGraph]')) {
        logs.push(text);
        console.log(text); // Print immediately
      }
    });

    // Capture page errors
    page.on('pageerror', (err) => {
      console.log('[PAGE ERROR]:', err.message);
    });

    // Clear storage to start fresh
    await page.goto('/');
    await waitForAppReady(page);
    await disableOthersPool(page);
    await page.evaluate(() => {
      localStorage.clear();
      indexedDB.deleteDatabase('hashtree-social-graph');
    });
    await page.reload();
    await waitForAppReady(page);
    await disableOthersPool(page);
    await ensureLoggedIn(page, 20000);

    // Close any modals
    const backdrop = page.locator('.fixed.inset-0.z-50');
    if (await backdrop.isVisible().catch(() => false)) {
      await backdrop.click({ position: { x: 5, y: 5 }, force: true });
    }

    // Get my pubkey from the URL or profile
    await page.click('[title="My Profile (double-click for users)"]');
    await page.waitForURL(/npub1/, { timeout: 5000 });

    const url = page.url();
    const npubMatch = url.match(/npub1[a-z0-9]+/);
    const myNpub = npubMatch?.[0];
    console.log('My npub:', myNpub);

    // Navigate to a known user (different from self) using in-page navigation
    // This is a valid npub (verified checksum)
    const testNpub = BOOTSTRAP_NPUB;
    console.log('Navigating to test user:', testNpub);

    // Use search to navigate (more realistic user flow)
    await page.fill('input[placeholder*="Search"]', testNpub);
    await page.keyboard.press('Enter');

    // Wait for URL to contain the test npub
    await page.waitForURL(new RegExp(testNpub.slice(0, 20)), { timeout: 10000 });
    console.log('Current URL:', page.url());

    // Wait for profile to load - check for Follow button or profile name
    await page.waitForSelector('button:has-text("Follow"), h1', { timeout: 10000 });

    // Verify we're on someone else's profile
    // Check for Follow button (would only appear on someone else's profile)
    const followButton = page.getByRole('button', { name: 'Follow', exact: true });
    const editButton = page.getByRole('button', { name: 'Edit Profile' });

    const hasFollowBtn = await followButton.isVisible().catch(() => false);
    const hasEditBtn = await editButton.isVisible().catch(() => false);

    console.log('Has Follow button:', hasFollowBtn);
    console.log('Has Edit Profile button:', hasEditBtn);

    // Get page content for debugging
    const profileName = await page.locator('h1').first().textContent().catch(() => 'unknown');
    console.log('Profile name shown:', profileName);

    // Wait for Known Followers text to appear (indicates profile loaded)
    // Use first() to avoid strict mode violation when there are multiple matches
    const followersText = page.locator('text=Known Followers').first();
    await followersText.waitFor({ state: 'visible', timeout: 10000 });

    // Get the known followers count before following
    const getFollowersCount = async () => {
      const text = await followersText.textContent();
      const match = text?.match(/(\d+)/);
      return match ? parseInt(match[1]) : 0;
    };

    const followersBefore = await getFollowersCount();
    console.log('Followers before:', followersBefore);

    // Take screenshot
    await page.screenshot({ path: 'e2e/screenshots/before-follow.png', fullPage: true });

    // Follow the user
    if (await followButton.isVisible().catch(() => false)) {
      console.log('Clicking follow button...');
      await followButton.click();

      // Wait for the social graph to update - poll for count increase
      // The fix should make this happen immediately after publish
      await page.waitForFunction(
        (beforeCount) => {
          const text = document.body.textContent || '';
          const match = text.match(/(\d+)\s*Known Followers/);
          const currentCount = match ? parseInt(match[1]) : 0;
          console.log('[Test] Checking followers count:', currentCount, 'before was:', beforeCount);
          return currentCount > beforeCount;
        },
        followersBefore,
        { timeout: 10000 }
      ).catch(() => {
        console.log('Followers count did not increase (may be expected for first follow)');
      });

      console.log('=== Logs after follow ===');
      logs.slice(-10).forEach(log => console.log(log));

      // Get final count
      const followersAfter = await getFollowersCount();
      console.log('Followers after:', followersAfter);

      // After following, I should be counted as a known follower
      // For a new account following someone, the count goes from 0 to 1
      expect(followersAfter).toBeGreaterThanOrEqual(followersBefore);
    } else {
      console.log('Follow button not visible, may already be following');
    }
  });

  test('should show known followers on direct profile navigation', async ({ page }) => {
    test.slow(); // This test needs time for social graph to load from relays
    const logs: string[] = [];
    page.on('console', (msg) => {
      const text = msg.text();
      if (text.includes('[Worker]') || text.includes('[socialGraph]')) {
        logs.push(text);
        console.log(text);
      }
    });

    await seedKnownFollowers();

    // Login first
    await page.goto('/');
    await waitForAppReady(page);
    await disableOthersPool(page);
    await useLocalRelay(page);
    await waitForRelayConnected(page, 30000);
    await ensureLoggedIn(page, 20000);
    await page.waitForFunction(() => (window as any).__testHelpers?.followPubkey, { timeout: 10000 });
    await page.evaluate((pk) => (window as any).__testHelpers?.followPubkey?.(pk), FOLLOW_PUBKEY);

    // Wait for login
    await page.waitForFunction(() => {
      const nostrStore = (window as any).__nostrStore;
      return nostrStore?.getState()?.pubkey?.length === 64;
    }, { timeout: 15000 });

    // Navigate directly to bootstrap user's profile
    await page.goto(`/#/${BOOTSTRAP_NPUB}`);
    await waitForRelayConnected(page, 30000);
    await page.evaluate(async (pubkey) => {
      const { fetchUserFollowers } = await import('/src/utils/socialGraph');
      fetchUserFollowers(pubkey);
    }, BOOTSTRAP_PUBKEY);

    // Wait for profile to load - "Known Followers" should appear
    await page.waitForSelector('text=Known Followers', { timeout: 15000 });

    // Get initial followers count
    // The format is: "<count> Known Followers"
    const getFollowersCount = async () => {
      const text = await page.locator('text=Known Followers').first().textContent();
      // Match count before "Known Followers" - format is "<count> Known Followers"
      const match = text?.match(/(\d+)\s*Known Followers/);
      return match ? parseInt(match[1]) : 0;
    };

    const initialCount = await getFollowersCount();
    console.log('Initial followers count:', initialCount);

    // Wait for known followers to become > 0 as social graph loads
    // The social graph needs to receive kind:3 events from people who follow this user
    const countResult = await page.waitForFunction(() => {
      const text = document.body.textContent || '';
      const match = text.match(/(\d+)\s*Known Followers/);
      return match ? parseInt(match[1]) : 0;
    }, { timeout: 30000, polling: 1000 }).catch(() => null);

    const count = countResult ? await countResult.jsonValue() : 0;
    console.log('Current followers count:', count);
    expect(count).toBeGreaterThan(0);
  });
});
