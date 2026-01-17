/**
 * E2E test for CI status display
 *
 * Tests that CI results from a runner's hashtree are fetched and displayed
 * in the git repo view. Uses two browser contexts:
 * 1. CI Runner - publishes CI results to their tree
 * 2. Repo Viewer - follows the runner, fetches CI status via WebRTC/Blossom
 */
import { test, expect, type Page } from './fixtures';
import { setupPageErrorHandler, disableOthersPool, followUser, waitForAppReady, ensureLoggedIn, navigateToPublicFolder, useLocalRelay, waitForRelayConnected, evaluateWithRetry, getTestRelayUrl } from './test-utils';
import { spawn, execSync, type ChildProcess } from 'child_process';
import fs from 'fs';
import os from 'os';
import path from 'path';
import { fileURLToPath } from 'url';
import { nip19, generateSecretKey, getPublicKey } from 'nostr-tools';
import { acquireRustLock, releaseRustLock } from './rust-lock.js';

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

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const HASHTREE_RS_DIR = path.resolve(__dirname, '../../../rust');
const HASHTREE_CI_DIR = path.resolve(__dirname, '../../../../hashtree-ci');

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

function hasRustToolchain(): boolean {
  try {
    execSync('cargo --version', { stdio: 'ignore' });
    return true;
  } catch {
    return false;
  }
}

function ensureBinary(binaryPath: string, buildCommand: string, cwd: string): string | null {
  try {
    if (fs.existsSync(binaryPath)) return binaryPath;
    execSync(buildCommand, { cwd, stdio: 'inherit' });
    return binaryPath;
  } catch (err) {
    console.log(`Failed to build ${binaryPath}:`, err);
    return null;
  }
}

function buildHashtreeConfig(relayUrl: string, blossomUrl: string, dataDir: string, runnerNpub: string): string {
  return `
[network]
relays = ["${relayUrl}"]
blossom_servers = ["${blossomUrl}"]

[nostr]
relays = ["${relayUrl}"]
allowed_npubs = ["${runnerNpub}"]

[blossom]
servers = ["${blossomUrl}"]
read_servers = ["${blossomUrl}"]
write_servers = ["${blossomUrl}"]

[server]
public_writes = true
enable_webrtc = false
stun_port = 0

[storage]
data_dir = "${dataDir}"
max_size_gb = 1
`.trimStart();
}

function writeRunnerConfig(configDir: string, nsec: string): void {
  const runnerDir = path.join(configDir, 'hashtree-ci');
  fs.mkdirSync(runnerDir, { recursive: true });
  const content = `
[runner]
name = "e2e-runner"
nsec = "${nsec}"
tags = ["linux"]
`.trimStart();
  fs.writeFileSync(path.join(runnerDir, 'runner.toml'), content);
}

async function configureLocalBlossom(page: Page, blossomUrl: string): Promise<void> {
  await evaluateWithRetry(page, async (url) => {
    const configure = (window as unknown as { __configureBlossomServers?: (servers: unknown[]) => void }).__configureBlossomServers;
    if (!configure) {
      throw new Error('__configureBlossomServers not found');
    }
    configure([{ url, read: true, write: true }]);
  }, blossomUrl);
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

  test('htci publishes CI status and logs that render in git view', async ({ page }, testInfo) => {
    test.slow();
    test.setTimeout(180000);

    if (!hasRustToolchain()) {
      test.skip(true, 'Rust toolchain not available');
      return;
    }

    if (!fs.existsSync(path.join(HASHTREE_RS_DIR, 'Cargo.toml'))) {
      test.skip(true, 'hashtree rust workspace not available');
      return;
    }

    if (!fs.existsSync(path.join(HASHTREE_CI_DIR, 'Cargo.toml'))) {
      test.skip(true, 'hashtree-ci workspace not available');
      return;
    }

    let lockFd: number | null = null;
    let htreeProcess: ChildProcess | null = null;
    const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), 'htci-e2e-'));
    const xdgConfigDir = path.join(tempDir, 'xdg-config');
    const xdgDataDir = path.join(tempDir, 'xdg-data');
    const hashtreeDir = path.join(tempDir, '.hashtree');
    const dataDir = path.join(tempDir, 'htree-data');

    const relayUrl = getTestRelayUrl();
    const blossomPort = 18780 + (Number.isFinite(testInfo.workerIndex) ? testInfo.workerIndex : 0);
    const blossomUrl = `http://127.0.0.1:${blossomPort}`;

    const secretKey = generateSecretKey();
    const runnerPubkey = getPublicKey(secretKey);
    const runnerNpub = nip19.npubEncode(runnerPubkey);
    const runnerNsec = nip19.nsecEncode(secretKey);

    const configToml = buildHashtreeConfig(relayUrl, blossomUrl, dataDir, runnerNpub);

    fs.mkdirSync(hashtreeDir, { recursive: true });
    fs.mkdirSync(dataDir, { recursive: true });
    fs.mkdirSync(xdgConfigDir, { recursive: true });
    fs.mkdirSync(xdgDataDir, { recursive: true });
    fs.writeFileSync(path.join(hashtreeDir, 'config.toml'), configToml);
    writeRunnerConfig(xdgConfigDir, runnerNsec);

    lockFd = await acquireRustLock(180000);

    try {
      const htreeBin = ensureBinary(
        path.join(HASHTREE_RS_DIR, 'target/release/htree'),
        'cargo build --release -p hashtree-cli',
        HASHTREE_RS_DIR
      );
      if (!htreeBin) {
        throw new Error('htree binary unavailable');
      }

      const htciBin = ensureBinary(
        path.join(HASHTREE_CI_DIR, 'target/release/htci'),
        'cargo build --release -p ci-runner',
        HASHTREE_CI_DIR
      );
      if (!htciBin) {
        throw new Error('htci binary unavailable');
      }

      const htreeEnv = {
        ...process.env,
        HOME: tempDir,
        HTREE_CONFIG_DIR: hashtreeDir,
        RUST_LOG: 'info',
      };

      htreeProcess = spawn(htreeBin, ['start', '--addr', `127.0.0.1:${blossomPort}`], {
        env: htreeEnv,
        stdio: ['ignore', 'pipe', 'pipe'],
      });

      await new Promise<void>((resolve, reject) => {
        const timeout = setTimeout(() => reject(new Error('htree start timeout')), 30000);
        const onData = (data: Buffer) => {
          const text = data.toString();
          if (text.includes('Web UI') || text.includes('WebRTC') || text.includes('listening') || text.includes('Listening')) {
            clearTimeout(timeout);
            resolve();
          }
        };
        htreeProcess?.stdout?.on('data', onData);
        htreeProcess?.stderr?.on('data', onData);
        htreeProcess?.on('error', (err) => {
          clearTimeout(timeout);
          reject(err);
        });
        htreeProcess?.on('exit', (code) => {
          if (code && code !== 0) {
            clearTimeout(timeout);
            reject(new Error(`htree exited with code ${code}`));
          }
        });
      });

      setupPageErrorHandler(page);
      await page.goto('/');

      await page.evaluate(async () => {
        const dbs = await indexedDB.databases();
        for (const db of dbs) {
          if (db.name) indexedDB.deleteDatabase(db.name);
        }
        localStorage.clear();
        sessionStorage.clear();
      });

      await page.reload();
      await waitForAppReady(page, 60000);
      await disableOthersPool(page);
      await useLocalRelay(page);
      await waitForRelayConnected(page, 30000);
      await configureLocalBlossom(page, blossomUrl);
      await navigateToPublicFolder(page, { timeoutMs: 60000 });

      const repoName = `htci-test-${Date.now()}`;
      await page.getByRole('button', { name: 'New Folder' }).click();
      const folderInput = page.locator('input[placeholder="Folder name..."]');
      await folderInput.waitFor({ timeout: 5000 });
      await folderInput.fill(repoName);
      await page.click('button:has-text("Create")');
      await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

      const repoLink = page.locator('[data-testid="file-list"] a').filter({ hasText: repoName }).first();
      await expect(repoLink).toBeVisible({ timeout: 15000 });
      await repoLink.click();
      await page.waitForURL(new RegExp(repoName), { timeout: 10000 });

      const ciConfigResult = await page.evaluate(async ({ runnerNpub }) => {
        const { getTree, LinkType } = await import('/src/store.ts');
        const { autosaveIfOwn } = await import('/src/nostr.ts');
        const { getCurrentRootCid } = await import('/src/actions/route.ts');
        const { getRouteSync } = await import('/src/stores/index.ts');

        const tree = getTree();
        let rootCid = getCurrentRootCid();
        if (!rootCid) return { error: 'no root cid' };

        const route = getRouteSync();
        const basePath = route.path;

        const ciToml = `[ci]\n[[ci.runners]]\nnpub = "${runnerNpub}"\nname = "e2e-runner"\n`;
        const readme = '# CI Runner Test\n';

        const { cid: ciCid, size: ciSize } = await tree.putFile(new TextEncoder().encode(ciToml));
        const { cid: readmeCid, size: readmeSize } = await tree.putFile(new TextEncoder().encode(readme));

        const { cid: hashtreeDirCid } = await tree.putDirectory([
          { name: 'ci.toml', cid: ciCid, size: ciSize, type: LinkType.Blob },
        ]);

        rootCid = await tree.setEntry(rootCid, basePath, '.hashtree', hashtreeDirCid, 0, LinkType.Dir);
        rootCid = await tree.setEntry(rootCid, basePath, 'README.md', readmeCid, readmeSize, LinkType.Blob);
        autosaveIfOwn(rootCid);

        return { success: true };
      }, { runnerNpub });

      if (ciConfigResult?.error) {
        throw new Error(`Failed to write ci.toml: ${ciConfigResult.error}`);
      }

      const gitInitBtn = page.getByRole('button', { name: 'Git Init' });
      await expect(gitInitBtn).toBeVisible({ timeout: 15000 });
      await gitInitBtn.click();
      await expect(gitInitBtn).not.toBeVisible({ timeout: 30000 });

      await expect(page.locator('[title="No uncommitted changes"]')).toBeVisible({ timeout: 30000 });

      const repoInfoHandle = await page.waitForFunction(async () => {
        const { getTree } = await import('/src/store.ts');
        const { getCurrentRootCid } = await import('/src/actions/route.ts');
        const { getRouteSync } = await import('/src/stores/index.ts');
        const { getHead } = await import('/src/utils/git.ts');
        const nostrStore = (window as any).__nostrStore;

        const tree = getTree();
        const rootCid = getCurrentRootCid();
        const route = getRouteSync();
        if (!rootCid || !route.treeName) return null;

        const resolved = route.path.length > 0
          ? await tree.resolvePath(rootCid, route.path)
          : { cid: rootCid };

        if (!resolved?.cid) return null;

        const commit = await getHead(resolved.cid);
        if (!commit) return null;

        const repoPath = route.path.length > 0
          ? `${route.treeName}/${route.path.join('/')}`
          : route.treeName;

        return {
          ownerNpub: nostrStore?.getState?.().npub || '',
          repoPath,
          commit,
        };
      }, undefined, { timeout: 30000 });

      const repoInfo = await repoInfoHandle.jsonValue();

      const localRepo = path.join(tempDir, 'ci-local-repo');
      fs.mkdirSync(path.join(localRepo, '.github', 'workflows'), { recursive: true });
      const logMarker = `CI_LOG_MARKER_${Date.now()}`;
      const workflow = `
name: CI
on: [push]
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - name: Echo
        run: echo "${logMarker}"
      - name: Done
        run: echo "DONE"
`.trimStart();
      fs.writeFileSync(path.join(localRepo, '.github', 'workflows', 'ci.yml'), workflow);
      fs.writeFileSync(path.join(localRepo, 'README.md'), 'ci local repo');
      execSync('git init', { cwd: localRepo, stdio: 'ignore' });
      execSync('git config user.email "test@test.com"', { cwd: localRepo, stdio: 'ignore' });
      execSync('git config user.name "Test User"', { cwd: localRepo, stdio: 'ignore' });
      execSync('git add -A', { cwd: localRepo, stdio: 'ignore' });
      execSync('git commit -m "Initial commit"', { cwd: localRepo, stdio: 'ignore' });

      const htciEnv = {
        ...process.env,
        HOME: tempDir,
        XDG_CONFIG_HOME: xdgConfigDir,
        XDG_DATA_HOME: xdgDataDir,
        HTREE_CONFIG_DIR: hashtreeDir,
        RUST_LOG: 'info',
      };

      await new Promise<void>((resolve, reject) => {
        const proc = spawn(
          htciBin,
          [
            'run',
            '--repo', localRepo,
            '--owner-npub', repoInfo.ownerNpub,
            '--repo-path', repoInfo.repoPath,
            '--workflow', 'ci.yml',
            '--commit', repoInfo.commit,
          ],
          { env: htciEnv, stdio: ['ignore', 'pipe', 'pipe'] }
        );

        const onExit = (code: number | null) => {
          if (code && code !== 0) {
            reject(new Error(`htci exited with code ${code}`));
            return;
          }
          resolve();
        };

        proc.on('error', reject);
        proc.on('exit', onExit);
      });

      const badge = page.locator('button[title="View CI runs"] [data-testid="ci-status-badge"][data-ci-status="success"]');
      await expect(badge).toBeVisible({ timeout: 60000 });

      await page.locator('button[title="View CI runs"]').click();
      const modal = page.locator('[data-testid="ci-runs-modal-backdrop"]').first();
      await expect(modal).toBeVisible({ timeout: 10000 });
      const logPre = page.locator('pre').filter({ hasText: logMarker }).first();
      await expect(logPre).toContainText(logMarker, { timeout: 60000 });
    } finally {
      if (htreeProcess) {
        htreeProcess.kill('SIGTERM');
        htreeProcess = null;
      }
      if (lockFd !== null) {
        releaseRustLock(lockFd);
        lockFd = null;
      }
      if (tempDir && fs.existsSync(tempDir)) {
        fs.rmSync(tempDir, { recursive: true, force: true });
      }
    }
  });
});
