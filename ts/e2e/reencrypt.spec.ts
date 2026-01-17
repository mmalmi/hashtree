import { test, expect } from './fixtures';
import { setupPageErrorHandler, disableOthersPool, waitForAppReady, ensureLoggedIn } from './test-utils';
// Tests use isolated page contexts with disableOthersPool - safe for parallel execution

/**
 * Re-encryption tests
 *
 * Tests that unencrypted data gets re-encrypted before upload to Blossom
 */

test.describe('Re-encryption', () => {
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
  });

  test('detects unencrypted tree and triggers re-encryption', async ({ page }) => {
    test.slow();

    await page.goto('/');
    await waitForAppReady(page);
    await ensureLoggedIn(page);
    await disableOthersPool(page);

    // Create a test tree with unencrypted data by using the store directly
    const hasUnencryptedTree = await page.evaluate(async () => {
      // Access the store
      const { getTree } = await import('/src/store.ts');
      const tree = getTree();

      // Create unencrypted file
      const data = new TextEncoder().encode('test unencrypted data ' + Date.now());
      const { cid, size } = await tree.putFile(data, { unencrypted: true });

      // Check if key is undefined (unencrypted)
      return !cid.key;
    });

    expect(hasUnencryptedTree).toBe(true);
    console.log('Created unencrypted tree');

    // Listen for re-encryption logs
    const logs: string[] = [];
    page.on('console', msg => {
      if (msg.text().includes('[Reencrypt]') || msg.text().includes('[BlossomPush]')) {
        logs.push(msg.text());
      }
    });

    // Try to push - should detect no key and trigger re-encryption
    // This would be triggered by the UI, but for now just verify the detection logic
    const detectionResult = await page.evaluate(async () => {
      const { getTree } = await import('/src/store.ts');
      const tree = getTree();

      // Create unencrypted file
      const data = new TextEncoder().encode('test data');
      const { cid } = await tree.putFile(data, { unencrypted: true });

      // Check: no key = unencrypted
      const needsEncryption = !cid.key;

      // Re-encrypt: read without key, store with encryption
      if (needsEncryption) {
        const readData = await tree.readFile({ hash: cid.hash });
        if (readData) {
          const { cid: encryptedCid } = await tree.putFile(readData);
          return {
            originalHasKey: !!cid.key,
            encryptedHasKey: !!encryptedCid.key,
            success: !!encryptedCid.key
          };
        }
      }
      return { originalHasKey: !!cid.key, encryptedHasKey: false, success: false };
    });

    console.log('Detection result:', detectionResult);
    expect(detectionResult.originalHasKey).toBe(false);

    expect(detectionResult.encryptedHasKey).toBe(true);
    expect(detectionResult.success).toBe(true);
  });

  test('re-encrypted data has high entropy', async ({ page }) => {
    test.slow();

    await page.goto('/');
    await waitForAppReady(page);
    await ensureLoggedIn(page);
    await disableOthersPool(page);

    const entropyResult = await page.evaluate(async () => {
      const { getTree } = await import('/src/store.ts');
      const { getWorkerStore } = await import('/src/stores/workerStore.ts');
      const tree = getTree();
      const store = getWorkerStore();

      // Create unencrypted data (256+ bytes for meaningful entropy check)
      const data = new Uint8Array(300);
      for (let i = 0; i < data.length; i++) {
        data[i] = i % 256; // Low entropy pattern
      }

      // Store unencrypted
      const { cid: unencryptedCid } = await tree.putFile(data, { unencrypted: true });
      const unencryptedBlob = await store.get(unencryptedCid.hash);

      // Count unique bytes in unencrypted
      const countUnique = (arr: Uint8Array) => {
        const size = Math.min(arr.length, 256);
        const seen = new Set<number>();
        for (let i = 0; i < size; i++) seen.add(arr[i]);
        return seen.size;
      };

      const unencryptedEntropy = unencryptedBlob ? countUnique(unencryptedBlob) : 0;

      // Re-encrypt
      const readData = await tree.readFile({ hash: unencryptedCid.hash });
      if (!readData) return { success: false, reason: 'Could not read unencrypted data' };

      const { cid: encryptedCid } = await tree.putFile(readData);
      const encryptedBlob = await store.get(encryptedCid.hash);

      const encryptedEntropy = encryptedBlob ? countUnique(encryptedBlob) : 0;

      return {
        success: true,
        unencryptedEntropy,
        encryptedEntropy,
        encryptedHasKey: !!encryptedCid.key,
        // Encrypted data should have higher entropy (> 111 for Blossom)
        passesBlossomCheck: encryptedEntropy >= 111
      };
    });

    console.log('Entropy result:', entropyResult);

    expect(entropyResult.success).toBe(true);
    expect(entropyResult.encryptedHasKey).toBe(true);
    // Encrypted data should pass Blossom's entropy check (>= 111 unique bytes)
    expect(entropyResult.passesBlossomCheck).toBe(true);
    expect(entropyResult.encryptedEntropy).toBeGreaterThanOrEqual(111);
  });
});
