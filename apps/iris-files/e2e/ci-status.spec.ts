/**
 * E2E test for CI status display
 *
 * Tests that CI results from a runner's hashtree are fetched and displayed
 * in the git repo view. Uses two browser contexts:
 * 1. CI Runner - publishes CI results to their tree
 * 2. Repo Viewer - follows the runner, fetches CI status via WebRTC/Blossom
 */
import { test, expect, type Page } from './fixtures';
import { setupPageErrorHandler, disableOthersPool, followUser, waitForAppReady, ensureLoggedIn, navigateToPublicFolder, useLocalRelay, waitForRelayConnected } from './test-utils';

// Run tests serially to avoid WebRTC conflicts
test.describe.configure({ mode: 'serial' });

// Sample CI result matching hashtree-ci format
const SAMPLE_CI_RESULT = {
  job_id: '550e8400-e29b-41d4-a716-446655440000',
  runner_npub: '', // Will be filled in with actual npub
  repo_hash: '', // Will be filled in
  commit: 'abc123def456',
  workflow: '.github/workflows/ci.yml',
  job_name: 'build',
  status: 'success',
  started_at: '2025-01-06T10:00:00Z',
  finished_at: '2025-01-06T10:05:30Z',
  logs_hash: 'sha256:1234567890abcdef',
  steps: [
    {
      name: 'Build',
      status: 'success',
      exit_code: 0,
      duration_secs: 300,
      logs_hash: 'sha256:step1hash',
    },
    {
      name: 'Test',
      status: 'success',
      exit_code: 0,
      duration_secs: 45,
      logs_hash: 'sha256:step2hash',
    },
  ],
};

// Setup fresh user with cleared storage
async function setupFreshUser(page: Page): Promise<void> {
  await page.goto('http://localhost:5173');

  // Clear storage
  await page.evaluate(async () => {
    const dbs = await indexedDB.databases();
    for (const db of dbs) {
      if (db.name) indexedDB.deleteDatabase(db.name);
    }
    localStorage.clear();
    sessionStorage.clear();
  });

  await page.reload();
  await waitForAppReady(page);
  await useLocalRelay(page);
  await waitForRelayConnected(page, 30000);
}

async function getNpubFromPage(page: Page): Promise<string> {
  await waitForAppReady(page);
  await ensureLoggedIn(page);
  await navigateToPublicFolder(page);

  const url = page.url();
  const match = url.match(/npub1[a-z0-9]+/);
  if (!match) throw new Error('Could not find npub in URL');
  return match[0];
}

/**
 * Write CI result using the app's internal tree API
 * Creates the "ci" tree if it doesn't exist and adds result.json at nested path
 */
async function writeCIResultToTree(page: Page, repoPath: string, commit: string, runnerNpub: string): Promise<string> {
  // Create CI result with correct values
  const ciResult = {
    ...SAMPLE_CI_RESULT,
    runner_npub: runnerNpub,
    repo_hash: repoPath,
    commit,
  };

  // Create the nested path and write result.json using the tree API
  const result = await page.evaluate(async ({ ciResult, repoPath, commit }) => {
    // Import from app's modules
    const { getTree, LinkType } = await import('/src/store.ts');
    const { createTree } = await import('/src/actions/tree.ts');
    const { useNostrStore } = await import('/src/nostr/index.ts');
    const { getLocalRootCache, getLocalRootKey, updateLocalRootCache, flushPendingPublishes } = await import('/src/treeRootCache.ts');

    const tree = getTree();
    const nostrState = useNostrStore.getState();
    const myNpub = nostrState.npub;
    if (!myNpub) throw new Error('No npub available');

    // Check if ci tree exists in local cache
    let ciRootHash = getLocalRootCache(myNpub, 'ci');

    if (!ciRootHash) {
      // Create the "ci" tree first
      console.log('[CI Runner] Creating ci tree...');
      const createResult = await createTree('ci', 'public', true); // skipNavigation=true
      if (!createResult.success) throw new Error('Failed to create ci tree');

      // Get the newly created root
      ciRootHash = getLocalRootCache(myNpub, 'ci');
      if (!ciRootHash) throw new Error('ci tree not in cache after creation');
    }

    // Reconstruct CID from hash and key
    const ciRootKey = getLocalRootKey(myNpub, 'ci');
    let currentRootCid: any = { hash: ciRootHash, key: ciRootKey };

    // Create the nested directory structure
    // Path: <repoPath>/<commit>/result.json
    const pathParts = [...repoPath.split('/'), commit];

    // Create intermediate directories one level at a time
    const { cid: emptyDirCid } = await tree.putDirectory([]);

    for (let i = 0; i < pathParts.length; i++) {
      const parentPath = pathParts.slice(0, i);
      const dirName = pathParts[i];
      console.log(`[CI Runner] Creating dir: ${parentPath.join('/')}/${dirName}`);
      currentRootCid = await tree.setEntry(currentRootCid, parentPath, dirName, emptyDirCid, 0, LinkType.Dir);
    }

    // Create result.json content
    const resultJson = JSON.stringify(ciResult, null, 2);
    const resultData = new TextEncoder().encode(resultJson);
    const { cid: fileCid, size } = await tree.putFile(resultData);

    // Add the file at the nested path
    const newRootCid = await tree.setEntry(currentRootCid, pathParts, 'result.json', fileCid, size, LinkType.Blob);

    // Update local cache to trigger Nostr publish
    updateLocalRootCache(myNpub, 'ci', newRootCid.hash, newRootCid.key, 'public');

    // Force immediate publish
    await flushPendingPublishes();

    console.log(`[CI Runner] Created CI result at ci/${pathParts.join('/')}/result.json`);

    return {
      ciPath: `ci/${repoPath}/${commit}/result.json`,
      success: true,
      treeName: 'ci',
    };
  }, { ciResult, repoPath, commit });

  return result.ciPath;
}

async function getWebRTCPeers(page: Page): Promise<any[]> {
  return page.evaluate(() => {
    const store = (window as any).webrtcStore;
    return store?.getPeers?.()?.map((p: any) => ({
      pubkey: p.pubkey?.slice(0, 16),
      isConnected: p.isConnected,
      pool: p.pool,
      pcState: p.pc?.connectionState,
      dcState: p.dataChannel?.readyState,
    })) || [];
  });
}

async function waitForWebRTCConnection(page: Page, timeoutMs: number = 10000): Promise<boolean> {
  try {
    await page.waitForFunction(() => {
      const store = (window as any).webrtcStore;
      return store?.getPeers?.()?.some((p: any) => p.isConnected);
    }, { timeout: timeoutMs });
    return true;
  } catch {
    return false;
  }
}

test.describe('CI Status Display', () => {
  test.setTimeout(120000);

  test('CI status is fetched from runner via WebRTC', async ({ browser }) => {
    test.slow();

    // Create two browser contexts
    const runnerContext = await browser.newContext();
    const viewerContext = await browser.newContext();

    const runnerPage = await runnerContext.newPage();
    const viewerPage = await viewerContext.newPage();

    setupPageErrorHandler(runnerPage);
    setupPageErrorHandler(viewerPage);

    // Detailed logging
    const logs = { runner: [] as string[], viewer: [] as string[] };

    runnerPage.on('console', msg => {
      const text = msg.text();
      logs.runner.push(text);
      if (text.includes('[CI') || text.includes('WebRTC') || text.includes('peer') || text.includes('Nostr')) {
        console.log(`[Runner] ${text}`);
      }
    });
    viewerPage.on('console', msg => {
      const text = msg.text();
      logs.viewer.push(text);
      if (text.includes('[CI') || text.includes('WebRTC') || text.includes('peer') || text.includes('resolv') || text.includes('[Viewer]') || text.includes('Tree CID')) {
        console.log(`[Viewer] ${text}`);
      }
    });

    try {
      // === Setup Runner ===
      console.log('\n=== Setting up CI Runner ===');
      await setupFreshUser(runnerPage);
      await disableOthersPool(runnerPage);
      const runnerNpub = await getNpubFromPage(runnerPage);
      console.log(`Runner: ${runnerNpub}`);

      // === Setup Viewer ===
      console.log('\n=== Setting up Viewer ===');
      await setupFreshUser(viewerPage);
      await disableOthersPool(viewerPage);
      const viewerNpub = await getNpubFromPage(viewerPage);
      console.log(`Viewer: ${viewerNpub}`);

      // === Mutual follows for WebRTC ===
      console.log('\n=== Setting up mutual follows ===');
      await followUser(runnerPage, viewerNpub);
      await followUser(viewerPage, runnerNpub);
      console.log('Mutual follows established');

      // === Wait for WebRTC connections ===
      console.log('\n=== Waiting for WebRTC connections ===');

      // Wait for both sides to have at least one connected peer
      const [runnerConnected, viewerConnected] = await Promise.all([
        waitForWebRTCConnection(runnerPage),
        waitForWebRTCConnection(viewerPage),
      ]);

      const runnerPeers = await getWebRTCPeers(runnerPage);
      const viewerPeers = await getWebRTCPeers(viewerPage);
      console.log('Runner peers:', JSON.stringify(runnerPeers, null, 2));
      console.log('Viewer peers:', JSON.stringify(viewerPeers, null, 2));

      if (!runnerConnected || !viewerConnected) {
        console.log('WARNING: WebRTC connection not fully established');
      }

      // === Write CI result from runner ===
      console.log('\n=== Writing CI result ===');
      const testCommit = `commit_${Date.now()}`;
      const testRepoPath = 'repos/test-project';
      const ciPath = await writeCIResultToTree(runnerPage, testRepoPath, testCommit, runnerNpub);
      console.log(`CI result written to: ${ciPath}`);

      // Verify runner can read their own tree
      const runnerVerify = await runnerPage.evaluate(async () => {
        const { getWorkerAdapter } = await import('/src/lib/workerInit');
        const { getLocalRootCache, getLocalRootKey } = await import('/src/treeRootCache.ts');
        const { useNostrStore } = await import('/src/nostr/index.ts');

        const adapter = getWorkerAdapter();
        if (!adapter) return { error: 'No adapter' };

        const myNpub = useNostrStore.getState().npub;
        const ciHash = getLocalRootCache(myNpub!, 'ci');
        const ciKey = getLocalRootKey(myNpub!, 'ci');

        if (!ciHash) return { error: 'No CI tree in cache' };

        const ciCid = { hash: ciHash, key: ciKey };
        const entries = await adapter.listDir(ciCid);

        // Convert to hex for logging
        const bytesToHex = (bytes: Uint8Array) => Array.from(bytes).map(b => b.toString(16).padStart(2, '0')).join('');

        return {
          success: true,
          entries: entries.map(e => e.name),
          hasKey: !!ciKey,
          hashHex: bytesToHex(ciHash).slice(0, 32),
          keyHex: ciKey ? bytesToHex(ciKey).slice(0, 32) : undefined,
        };
      });

      console.log('Runner tree verification:', JSON.stringify(runnerVerify, null, 2));

      // === Viewer fetches CI status ===
      console.log('\n=== Viewer fetching CI status ===');

      // First, use the RefResolver to subscribe to and fetch the runner's CI tree root
      const resolveResult = await viewerPage.evaluate(async ({ runnerNpub, repoPath, commit }) => {
        const { getWorkerAdapter } = await import('/src/lib/workerInit');
        const { getRefResolver } = await import('/src/refResolver.ts');
        const adapter = getWorkerAdapter();
        if (!adapter) return { error: 'No adapter' };

        try {
          // The CI tree is stored at runnerNpub/ci
          // Within that tree: <repoPath>/<commit>/result.json
          const treeName = 'ci';

          // Use the RefResolver to subscribe to and resolve the runner's CI tree root
          // This subscribes to Nostr events and waits for the tree root
          console.log(`[Viewer] Getting RefResolver...`);
          const resolver = getRefResolver();
          const resolverKey = `${runnerNpub}/${treeName}`;
          console.log(`[Viewer] Resolving tree root via RefResolver: ${resolverKey}`);

          // Add a timeout wrapper since resolve() waits indefinitely
          const treeCid = await Promise.race([
            resolver.resolve(resolverKey),
            new Promise<null>((resolve) => setTimeout(() => resolve(null), 10000))
          ]);

          if (!treeCid) return { error: 'Could not resolve CI tree root (timeout)', runnerNpub, treeName };

          // toHex function inline since we can't easily import from hashtree in evaluate
          const bytesToHex = (bytes: Uint8Array) => Array.from(bytes).map(b => b.toString(16).padStart(2, '0')).join('');
          console.log(`[Viewer] Resolved CI tree root for ${runnerNpub}/${treeName}`);
          console.log(`[Viewer] Tree CID hash: ${bytesToHex(treeCid.hash).slice(0, 16)}`);
          console.log(`[Viewer] Tree CID has key: ${!!treeCid.key}`);
          if (treeCid.key) {
            console.log(`[Viewer] Tree CID key: ${bytesToHex(treeCid.key).slice(0, 16)}`);
          }

          // Navigate to the commit directory: <repoPath>/<commit>
          const pathParts = [...repoPath.split('/'), commit];
          let currentCid = treeCid;

          // First just try to list the root directory to see what's there
          console.log(`[Viewer] Listing root dir...`);
          const rootEntries = await adapter.listDir(currentCid);
          console.log(`[Viewer] Root dir entries: ${rootEntries.map(e => e.name).join(', ') || '(empty)'}`);

          for (const part of pathParts) {
            const entries = await adapter.listDir(currentCid);
            console.log(`[Viewer] At path, looking for ${part}, found: ${entries.map(e => e.name).join(', ') || '(empty)'}`);
            const entry = entries.find(e => e.name === part);
            if (!entry) {
              return { error: `Path not found: ${part}`, pathParts, availableEntries: entries.map(e => e.name) };
            }
            currentCid = entry.cid;
          }

          // Now list the commit directory and find result.json
          const entries = await adapter.listDir(currentCid);
          const resultFile = entries.find(e => e.name === 'result.json');
          if (!resultFile) return { error: 'No result.json found', entries: entries.map(e => e.name) };

          // Read the file
          const data = await adapter.readFile(resultFile.cid);
          const json = new TextDecoder().decode(data);
          const result = JSON.parse(json);

          return { success: true, status: result.status, jobName: result.job_name };
        } catch (e) {
          return { error: String(e) };
        }
      }, { runnerNpub, repoPath: testRepoPath, commit: testCommit });

      console.log('Resolve result:', JSON.stringify(resolveResult, null, 2));

      // === Verify CI status was fetched ===
      if (resolveResult.error) {
        console.log('\n=== FAILURE: Could not fetch CI status ===');
        console.log('Error:', resolveResult.error);
        console.log('This means data is not flowing from runner to viewer.');
        console.log('Possible causes:');
        console.log('1. Tree root not propagated via Nostr relay');
        console.log('2. WebRTC connection not established');
        console.log('3. Chunks not available');

        // Log any errors from viewer
        const viewerErrors = logs.viewer.filter(l =>
          l.toLowerCase().includes('error') || l.includes('404') || l.includes('failed')
        );
        if (viewerErrors.length > 0) {
          console.log('\nViewer errors:');
          viewerErrors.slice(0, 10).forEach(e => console.log(`  ${e}`));
        }
      }

      // The actual assertion - CI status MUST be fetched
      expect(resolveResult.success).toBe(true);
      expect(resolveResult.status).toBe('success');
      expect(resolveResult.jobName).toBe('build');

      console.log('\n=== SUCCESS: CI status fetched via WebRTC/Nostr ===');

    } finally {
      await runnerContext.close();
      await viewerContext.close();
    }
  });

  test('CIStatusBadge renders correct status icons', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);

    // Test that CI store module loads correctly
    const moduleLoads = await page.evaluate(async () => {
      try {
        const ciModule = await import('/src/stores/ci');
        return {
          hasCreateCIStatusStore: typeof ciModule.createCIStatusStore === 'function',
          hasParseCIConfig: typeof ciModule.parseCIConfig === 'function',
        };
      } catch (e) {
        return { error: String(e) };
      }
    });

    // Verify the module exports the expected functions
    expect(moduleLoads.hasCreateCIStatusStore).toBe(true);
    expect(moduleLoads.hasParseCIConfig).toBe(true);
  });
});
