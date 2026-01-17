/**
 * Hashtree Worker
 *
 * Dedicated worker that owns:
 * - HashTree + DexieStore (IndexedDB storage)
 * - WebRTC peer connections (P2P data transfer)
 *
 * Main thread communicates via postMessage.
 * NIP-07 signing/encryption delegated back to main thread.
 */

import { HashTree } from '../../../../ts/packages/hashtree/src/hashtree';
import { DexieStore } from '../../../../ts/packages/hashtree/src/store/dexie';
import { BlossomStore } from '../../../../ts/packages/hashtree/src/store/blossom';
import { FallbackStore } from '../../../../ts/packages/hashtree/src/store/fallback';
import type { WorkerRequest, WorkerResponse, WorkerConfig, SignedEvent, WebRTCCommand, BlossomUploadProgress, BlossomServerStatus } from './protocol';
import { initTreeRootCache, getCachedRoot, clearMemoryCache } from './treeRootCache';
import {
  initNdk,
  closeNdk,
  subscribe as ndkSubscribe,
  unsubscribe as ndkUnsubscribe,
  publish as ndkPublish,
  setOnEvent,
  setOnEose,
  getRelayStats as getNdkRelayStats,
  republishTrees,
  republishTree,
} from './ndk';
import { initIdentity, setIdentity, clearIdentity } from './identity';
import {
  setResponseSender,
  signEvent,
  handleSignedResponse,
  handleEncryptedResponse,
  handleDecryptedResponse,
} from './signing';
import { WebRTCController } from './webrtc';
import { SocialGraph, type NostrEvent as SocialGraphNostrEvent } from 'nostr-social-graph';
import Dexie from 'dexie';
import { LRUCache } from '../utils/lruCache';
import { getErrorMessage } from '../utils/errorMessage';
import { DEFAULT_BOOTSTRAP_PUBKEY } from '../utils/constants';

// Dexie database for social graph persistence
class SocialGraphDB extends Dexie {
  socialGraph!: Dexie.Table<{ id: string; data: Uint8Array; updatedAt: number }>;

  constructor() {
    super('hashtree-social-graph');
    this.version(1).stores({
      socialGraph: '&id',
    });
  }
}

const socialGraphDB = new SocialGraphDB();
import { initMediaHandler, registerMediaPort } from './mediaHandler';
import {
  initWebRTCSignaling,
  sendWebRTCSignaling,
  setupWebRTCSignalingSubscription,
  handleWebRTCSignalingEvent,
} from './webrtcSignaling';
// Worker state
let tree: HashTree | null = null;
let store: DexieStore | null = null;
let fallbackStore: FallbackStore | null = null;
let blossomStore: BlossomStore | null = null;
let webrtc: WebRTCController | null = null;
let webrtcStarted = false;
let _config: WorkerConfig | null = null;
const WEBRTC_REQUEST_TIMEOUT_MS = 5000;

// Storage quota management
let storageMaxBytes = 1024 * 1024 * 1024; // Default 1GB
let evictionCheckPending = false;
const EVICTION_CHECK_DEBOUNCE_MS = 5000; // Debounce eviction checks

/**
 * Run storage eviction if over limit.
 * Debounced to avoid running too frequently after many puts.
 */
function runEvictionCheck(): void {
  if (evictionCheckPending || !store) return;

  evictionCheckPending = true;
  setTimeout(async () => {
    evictionCheckPending = false;
    if (!store) return;

    try {
      const evicted = await store.evict(storageMaxBytes);
      if (evicted > 0) {
        console.log(`[Worker] Eviction completed, removed ${evicted} entries`);
      }
    } catch (err) {
      console.error('[Worker] Eviction error:', err);
    }
  }, EVICTION_CHECK_DEBOUNCE_MS);
}

// Blossom upload progress tracking
interface BlossomProgressSession {
  sessionId: string;
  totalChunks: number;
  processedChunks: number;
  serverStatus: Map<string, BlossomServerStatus>;
  lastNotifyTime: number;
}
let blossomProgress: BlossomProgressSession | null = null;
const BLOSSOM_PROGRESS_THROTTLE_MS = 100; // Throttle progress updates to every 100ms

function initBlossomProgress(sessionId: string, totalChunks: number, serverUrls: string[]): void {
  const serverStatus = new Map<string, BlossomServerStatus>();
  for (const url of serverUrls) {
    serverStatus.set(url, { url, uploaded: 0, failed: 0, skipped: 0 });
  }
  blossomProgress = {
    sessionId,
    totalChunks,
    processedChunks: 0,
    serverStatus,
    lastNotifyTime: 0,
  };
}

function updateBlossomProgress(serverUrl: string, status: 'uploaded' | 'skipped' | 'failed'): void {
  if (!blossomProgress) return;

  const serverStat = blossomProgress.serverStatus.get(serverUrl);
  if (serverStat) {
    serverStat[status]++;
  }

  // Count as processed when all servers have responded for this chunk
  // We track per-server, so check if any server completed this chunk
  const allServersDone = Array.from(blossomProgress.serverStatus.values()).every(
    s => (s.uploaded + s.skipped + s.failed) > (blossomProgress!.processedChunks)
  );
  if (allServersDone) {
    blossomProgress.processedChunks++;
  }

  // Throttle progress updates
  const now = Date.now();
  if (now - blossomProgress.lastNotifyTime >= BLOSSOM_PROGRESS_THROTTLE_MS ||
      blossomProgress.processedChunks >= blossomProgress.totalChunks) {
    blossomProgress.lastNotifyTime = now;
    notifyBlossomProgress();
  }
}

function notifyBlossomProgress(): void {
  if (!blossomProgress) return;

  const progress: BlossomUploadProgress = {
    sessionId: blossomProgress.sessionId,
    totalChunks: blossomProgress.totalChunks,
    processedChunks: blossomProgress.processedChunks,
    servers: Array.from(blossomProgress.serverStatus.values()),
  };
  respond({ type: 'blossomUploadProgress', progress });
}

function clearBlossomProgress(): void {
  blossomProgress = null;
}

/**
 * Push a tree to blossom servers explicitly
 * Uses tree.push() to walk the tree and upload all chunks
 */
async function handlePushToBlossom(id: string, cidHash: Uint8Array, cidKey?: Uint8Array, treeName?: string): Promise<void> {
  if (!tree || !blossomStore) {
    respond({ type: 'blossomPushResult', id, pushed: 0, skipped: 0, failed: 0, error: 'Tree or BlossomStore not initialized' });
    return;
  }

  const cid = cidKey ? { hash: cidHash, key: cidKey } : { hash: cidHash };
  const name = treeName || 'unknown';
  let lastNotify = 0;

  try {
    console.log('[Worker] Starting blossom push for:', name);

    const result = await tree.push(cid, blossomStore, {
      onProgress: (current, total) => {
        // Update old blossom progress if session is active
        if (blossomProgress) {
          blossomProgress.processedChunks = current;
          blossomProgress.totalChunks = total;
          notifyBlossomProgress();
        }

        // Also emit new-style progress events (throttled)
        const now = Date.now();
        if (now - lastNotify >= 100 || current === total) {
          lastNotify = now;
          respond({ type: 'blossomPushProgress', treeName: name, current, total });
        }
      },
    });

    console.log('[Worker] Blossom push complete:', name, result);

    // Extract error messages from failed uploads
    const errorMessages = result.errors.map(e => e.error.message);

    // Emit completion event
    respond({
      type: 'blossomPushComplete',
      treeName: name,
      pushed: result.pushed,
      skipped: result.skipped,
      failed: result.failed,
    });

    respond({
      type: 'blossomPushResult',
      id,
      pushed: result.pushed,
      skipped: result.skipped,
      failed: result.failed,
      errors: errorMessages.length > 0 ? errorMessages : undefined,
    });
  } catch (err) {
    const error = getErrorMessage(err);
    console.error('[Worker] Blossom push failed:', error);
    respond({ type: 'blossomPushResult', id, pushed: 0, skipped: 0, failed: 0, error });
  }
}

/**
 * Count unique bytes in first 256 bytes (entropy check)
 */
function countUniqueBytes(data: Uint8Array): number {
  const sampleSize = Math.min(data.length, 256);
  const seen = new Set<number>();
  for (let i = 0; i < sampleSize; i++) {
    seen.add(data[i]);
  }
  return seen.size;
}

const ENTROPY_THRESHOLD = 111; // From Blossom error "Unique: 97 (min: 111)"

/**
 * Push to Blossom with progress reporting
 */
async function pushToBlossomWithProgress(
  treeName: string,
  hash: Uint8Array,
  key?: Uint8Array
): Promise<{ pushed: number; skipped: number; failed: number; errors: string[] }> {
  if (!tree || !blossomStore) {
    return { pushed: 0, skipped: 0, failed: 0, errors: [] };
  }

  const cid = key ? { hash, key } : { hash };
  let lastNotify = 0;

  // Pre-check: walk blocks and check entropy before uploading
  let lowEntropyCount = 0;
  for await (const block of tree.walkBlocks(cid)) {
    if (block.data.length >= 256) {
      const uniqueBytes = countUniqueBytes(block.data);
      if (uniqueBytes < ENTROPY_THRESHOLD) {
        lowEntropyCount++;
        console.warn(`[Worker] Low entropy blob detected: ${uniqueBytes} unique bytes (min: ${ENTROPY_THRESHOLD}), size: ${block.data.length}`);
      }
    }
  }

  if (lowEntropyCount > 0) {
    console.error(`[Worker] Found ${lowEntropyCount} low-entropy blobs in ${treeName} - data may not be encrypted!`);
    return {
      pushed: 0,
      skipped: 0,
      failed: lowEntropyCount,
      errors: [`Data not encrypted. Found ${lowEntropyCount} blobs with low entropy (< ${ENTROPY_THRESHOLD} unique bytes). Re-encryption required.`],
    };
  }

  const result = await tree.push(cid, blossomStore, {
    onProgress: (current, total) => {
      const now = Date.now();
      // Throttle to every 100ms
      if (now - lastNotify >= 100 || current === total) {
        lastNotify = now;
        respond({ type: 'blossomPushProgress', treeName, current, total });
      }
    },
  });

  const errorMessages = result.errors.map(e => e.error.message);

  respond({
    type: 'blossomPushComplete',
    treeName,
    pushed: result.pushed,
    skipped: result.skipped,
    failed: result.failed,
  });

  return { pushed: result.pushed, skipped: result.skipped, failed: result.failed, errors: errorMessages };
}

/**
 * Republish all cached tree events to relays and push blobs to Blossom
 * @param prefix - Optional URL-encoded prefix to filter trees by d-tag
 */
async function handleRepublishTrees(id: string, prefix?: string): Promise<void> {
  if (!_config?.pubkey) {
    respond({ type: 'republishResult', id, count: 0, error: 'Not logged in' });
    return;
  }

  // Track trees with encryption errors
  const encryptionErrors: string[] = [];

  // Create a push function that uses tree.push() with progress reporting
  const pushToBlossom = async (hash: Uint8Array, key?: Uint8Array, treeName?: string): Promise<{ pushed: number; skipped: number; failed: number }> => {
    // If no key, tree is unencrypted - add to encryption errors list
    if (!key) {
      console.log(`[Worker] Tree ${treeName} has no key - needs encryption`);
      encryptionErrors.push(treeName || 'unknown');
      return { pushed: 0, skipped: 0, failed: 1 };
    }
    const result = await pushToBlossomWithProgress(treeName || 'unknown', hash, key);
    // Check for encryption errors
    if (result.failed > 0 && result.errors.some(e => e.includes('not encrypted') || e.includes('Unique:'))) {
      encryptionErrors.push(treeName || 'unknown');
    }
    return result;
  };

  try {
    const count = await republishTrees(_config.pubkey, signEvent, pushToBlossom, prefix);
    respond({
      type: 'republishResult',
      id,
      count,
      encryptionErrors: encryptionErrors.length > 0 ? encryptionErrors : undefined,
    });
  } catch (err) {
    const error = getErrorMessage(err);
    console.error('[Worker] Republish failed:', error);
    respond({ type: 'republishResult', id, count: 0, error });
  }
}

async function handleRepublishTree(id: string, pubkey: string, treeName: string): Promise<void> {
  try {
    const success = await republishTree(pubkey, treeName);
    respond({ type: 'bool', id, value: success });
  } catch (err) {
    const error = getErrorMessage(err);
    console.error('[Worker] Republish tree failed:', error);
    respond({ type: 'bool', id, value: false, error });
  }
}

// Follows set for WebRTC peer classification
let followsSet = new Set<string>();

function getFollows(): Set<string> {
  return followsSet;
}

// SocialGraph state
const KIND_CONTACTS = 3;  // kind:3 = contact list
let socialGraph: SocialGraph = new SocialGraph(DEFAULT_BOOTSTRAP_PUBKEY);
let socialGraphVersion = 0;
let socialGraphSaveTimeout: ReturnType<typeof setTimeout> | null = null;
let socialGraphDirty = false;

function notifySocialGraphVersionUpdate() {
  socialGraphVersion++;
  socialGraphDirty = true;
  self.postMessage({ type: 'socialGraphVersion', version: socialGraphVersion });

  // Debounce save - save 5 seconds after last update
  if (socialGraphSaveTimeout) {
    clearTimeout(socialGraphSaveTimeout);
  }
  socialGraphSaveTimeout = setTimeout(() => {
    saveSocialGraph();
  }, 5000);
}

/**
 * Save social graph to IndexedDB
 */
async function saveSocialGraph(): Promise<void> {
  if (!socialGraphDirty) return;

  try {
    const data = await socialGraph.toBinary();
    await socialGraphDB.socialGraph.put({
      id: 'main',
      data,
      updatedAt: Date.now(),
    });
    socialGraphDirty = false;
    console.log('[Worker] Social graph saved to IndexedDB:', data.byteLength, 'bytes');
  } catch (err) {
    console.error('[Worker] Failed to save social graph:', err);
  }
}

// Track if social graph is still loading - don't subscribe until done
let socialGraphLoading = false;

/**
 * Load social graph from IndexedDB
 * Waits for completion (no timeout race condition)
 */
async function loadSocialGraph(rootPubkey: string): Promise<boolean> {
  socialGraphLoading = true;
  try {
    const row = await socialGraphDB.socialGraph.get('main');
    if (row?.data) {
      const loaded = await SocialGraph.fromBinary(rootPubkey, row.data);
      await socialGraph.merge(loaded);
      console.log('[Worker] Loaded social graph from IndexedDB, age:', Math.round((Date.now() - row.updatedAt) / 1000), 'seconds, size:', socialGraph.size());
      return true;
    }
    console.log('[Worker] No saved social graph found, starting fresh');
    return false;
  } catch (err) {
    console.error('[Worker] Failed to load social graph:', err);
    return false;
  } finally {
    socialGraphLoading = false;
  }
}

// Set up response sender for signing module
setResponseSender((msg) => self.postMessage(msg));

// ============================================================================
// Message Handler
// ============================================================================

self.onmessage = async (e: MessageEvent<WorkerRequest>) => {
  const msg = e.data;

  try {
    switch (msg.type) {
      // Lifecycle
      case 'init':
        await handleInit(msg.id, msg.config);
        break;
      case 'close':
        await handleClose(msg.id);
        break;
      case 'setIdentity':
        handleSetIdentity(msg.id, msg.pubkey, msg.nsec);
        break;

      // Heartbeat
      case 'ping':
        respond({ type: 'pong', id: msg.id });
        break;

      // Store operations
      case 'get':
        await handleGet(msg.id, msg.hash);
        break;
      case 'put':
        await handlePut(msg.id, msg.hash, msg.data);
        break;
      case 'has':
        await handleHas(msg.id, msg.hash);
        break;
      case 'delete':
        await handleDelete(msg.id, msg.hash);
        break;

      // Tree operations
      case 'readFile':
        await handleReadFile(msg.id, msg.cid);
        break;
      case 'readFileRange':
        await handleReadFileRange(msg.id, msg.cid, msg.start, msg.end);
        break;
      case 'readFileStream':
        await handleReadFileStream(msg.id, msg.cid);
        break;
      case 'writeFile':
        await handleWriteFile(msg.id, msg.parentCid, msg.path, msg.data);
        break;
      case 'deleteFile':
        await handleDeleteFile(msg.id, msg.parentCid, msg.path);
        break;
      case 'listDir':
        await handleListDir(msg.id, msg.cid);
        break;
      case 'resolveRoot':
        await handleResolveRoot(msg.id, msg.npub, msg.path);
        break;

      // Nostr (TODO: Phase 2)
      case 'subscribe':
        await handleSubscribe(msg.id, msg.filters);
        break;
      case 'unsubscribe':
        await handleUnsubscribe(msg.id, msg.subId);
        break;
      case 'publish':
        await handlePublish(msg.id, msg.event);
        break;

      // Media streaming
      case 'registerMediaPort':
        registerMediaPort(msg.port);
        break;

      // Stats
      case 'getPeerStats':
        await handleGetPeerStats(msg.id);
        break;
      case 'getRelayStats':
        await handleGetRelayStats(msg.id);
        break;
      case 'getStorageStats':
        await handleGetStorageStats(msg.id);
        break;

      // WebRTC pool configuration
      case 'setWebRTCPools':
        if (webrtc) {
          webrtc.setPoolConfig(msg.pools);
          // Start WebRTC on first pool config (waits for settings to load)
          if (!webrtcStarted) {
            webrtc.start();
            webrtcStarted = true;
            console.log('[Worker] WebRTC controller started (after pool config)');
          }
        }
        respond({ type: 'void', id: msg.id });
        break;
      case 'sendWebRTCHello':
        webrtc?.broadcastHello();
        respond({ type: 'void', id: msg.id });
        break;
      case 'setFollows':
        followsSet = new Set(msg.follows);
        console.log('[Worker] Follows updated:', followsSet.size, 'pubkeys:', Array.from(followsSet).map(p => p.slice(0, 16)));
        respond({ type: 'void', id: msg.id });
        break;

      // Blossom configuration
      case 'setBlossomServers':
        if (fallbackStore && blossomStore) {
          fallbackStore.removeFallback(blossomStore);
        }
        if (msg.servers && msg.servers.length > 0) {
          blossomStore = new BlossomStore({
            servers: msg.servers,
            signer: createBlossomSigner(),
            onUploadProgress: updateBlossomProgress,
          });
          fallbackStore?.addFallback(blossomStore);
          console.log('[Worker] BlossomStore updated with', msg.servers.length, 'servers');
        } else {
          blossomStore = null;
          console.log('[Worker] BlossomStore disabled (no servers configured)');
        }
        respond({ type: 'void', id: msg.id });
        break;

      // Storage quota configuration
      case 'setStorageMaxBytes':
        storageMaxBytes = msg.maxBytes;
        console.log('[Worker] Storage limit set to', Math.round(storageMaxBytes / 1024 / 1024), 'MB');
        // Run eviction check immediately when limit changes
        runEvictionCheck();
        respond({ type: 'void', id: msg.id });
        break;

      // Blossom upload
      case 'pushToBlossom':
        handlePushToBlossom(msg.id, msg.cidHash, msg.cidKey, msg.treeName);
        break;
      case 'startBlossomSession':
        {
          const serverUrls = blossomStore?.getWriteServers() || [];
          initBlossomProgress(msg.sessionId, msg.totalChunks, serverUrls);
          console.log('[Worker] Blossom session started:', msg.sessionId, 'chunks:', msg.totalChunks, 'servers:', serverUrls.length);
          respond({ type: 'void', id: msg.id });
        }
        break;
      case 'endBlossomSession':
        clearBlossomProgress();
        console.log('[Worker] Blossom session ended');
        respond({ type: 'void', id: msg.id });
        break;

      // Republish cached tree events
      case 'republishTrees':
        handleRepublishTrees(msg.id, msg.prefix);
        break;
      case 'republishTree':
        handleRepublishTree(msg.id, msg.pubkey, msg.treeName);
        break;

      // SocialGraph operations
      case 'initSocialGraph':
        handleInitSocialGraph(msg.id, msg.rootPubkey);
        break;
      case 'setSocialGraphRoot':
        handleSetSocialGraphRoot(msg.id, msg.pubkey);
        break;
      case 'handleSocialGraphEvents':
        handleSocialGraphEvents(msg.id, msg.events);
        break;
      case 'getFollowDistance':
        handleGetFollowDistance(msg.id, msg.pubkey);
        break;
      case 'isFollowing':
        handleIsFollowing(msg.id, msg.follower, msg.followed);
        break;
      case 'getFollows':
        handleGetFollowsList(msg.id, msg.pubkey);
        break;
      case 'getFollowers':
        handleGetFollowers(msg.id, msg.pubkey);
        break;
      case 'getFollowedByFriends':
        handleGetFollowedByFriends(msg.id, msg.pubkey);
        break;
      case 'fetchUserFollows':
        handleFetchUserFollows(msg.id, msg.pubkey);
        break;
      case 'fetchUserFollowers':
        handleFetchUserFollowers(msg.id, msg.pubkey);
        break;
      case 'getSocialGraphSize':
        handleGetSocialGraphSize(msg.id);
        break;

      // NIP-07 responses from main thread
      case 'signed':
        handleSignedResponse(msg.id, msg.event, msg.error);
        break;
      case 'encrypted':
        handleEncryptedResponse(msg.id, msg.ciphertext, msg.error);
        break;
      case 'decrypted':
        handleDecryptedResponse(msg.id, msg.plaintext, msg.error);
        break;

      // WebRTC proxy events from main thread
      case 'rtc:peerCreated':
      case 'rtc:peerStateChange':
      case 'rtc:peerClosed':
      case 'rtc:offerCreated':
      case 'rtc:answerCreated':
      case 'rtc:descriptionSet':
      case 'rtc:iceCandidate':
      case 'rtc:iceGatheringComplete':
      case 'rtc:dataChannelOpen':
      case 'rtc:dataChannelMessage':
      case 'rtc:dataChannelClose':
      case 'rtc:dataChannelError':
        webrtc?.handleProxyEvent(msg);
        break;

      default:
        console.warn('[Worker] Unknown message type:', (msg as { type: string }).type);
    }
  } catch (err) {
    const error = getErrorMessage(err);
    console.error('[Worker] Error handling message:', error);
    respond({ type: 'error', id: (msg as { id?: string }).id, error });
  }
};

// ============================================================================
// Response Helper
// ============================================================================

function respond(msg: WorkerResponse) {
  self.postMessage(msg);
}

function respondWithTransfer(msg: WorkerResponse, transfer: Transferable[]) {
  // Worker scope postMessage takes options object with transfer property
  self.postMessage(msg, { transfer });
}

// ============================================================================
// Lifecycle Handlers
// ============================================================================

async function handleInit(id: string, cfg: WorkerConfig) {
  try {
    _config = cfg;

    // Initialize Dexie/IndexedDB store
    const storeName = cfg.storeName || 'hashtree-worker';
    store = new DexieStore(storeName);

    // Initialize FallbackStore with local store (WebRTC and Blossom added dynamically)
    fallbackStore = new FallbackStore({ primary: store, fallbacks: [] });

    // Initialize HashTree with fallback store (enables remote fetching)
    tree = new HashTree({ store: fallbackStore });

    // Initialize tree root cache
    initTreeRootCache(store);

    console.log('[Worker] Initialized with DexieStore:', storeName);

    // Initialize identity
    initIdentity(cfg.pubkey, cfg.nsec);
    console.log('[Worker] User pubkey:', cfg.pubkey.slice(0, 16) + '...');

    // Initialize Blossom store with signer for uploads and progress callback
    if (cfg.blossomServers && cfg.blossomServers.length > 0) {
      blossomStore = new BlossomStore({
        servers: cfg.blossomServers,  // Pass full config with read/write flags
        signer: createBlossomSigner(),
        onUploadProgress: updateBlossomProgress,
      });
      // Add Blossom to fallback chain for remote chunk fetching
      fallbackStore?.addFallback(blossomStore);
      console.log('[Worker] Initialized BlossomStore with', cfg.blossomServers.length, 'servers');
    }

    // Initialize NDK with relays, cache, and nostr-wasm verification
    await initNdk(cfg.relays, {
      pubkey: cfg.pubkey,
      nsec: cfg.nsec,
    });
    console.log('[Worker] NDK initialized with', cfg.relays.length, 'relays');

    // Set up unified event handler for all subscriptions
    setOnEvent((subId, event) => {
      // Forward to main thread
      respond({ type: 'event', subId, event });

      // Route to WebRTC handler
      if (subId.startsWith('webrtc-')) {
        handleWebRTCSignalingEvent(event);
      }

      // Route to SocialGraph handler (all socialgraph-* subscriptions)
      if (subId.startsWith('socialgraph-') && event.kind === KIND_CONTACTS) {
        handleSocialGraphEvent(event);
      }
    });

    // Set up EOSE handler
    setOnEose((subId) => {
      respond({ type: 'eose', subId });
    });

    // Initialize WebRTC controller (RTCPeerConnection runs in main thread proxy)
    webrtc = new WebRTCController({
      pubkey: cfg.pubkey,
      localStore: store,
      sendCommand: (cmd: WebRTCCommand) => respond(cmd),
      sendSignaling: async (msg, recipientPubkey) => {
        await sendWebRTCSignaling(msg, recipientPubkey);
      },
      getFollows, // Used to classify peers into follows/other pools
      debug: false,
      requestTimeout: WEBRTC_REQUEST_TIMEOUT_MS,
    });

    // Add WebRTC to fallback chain for remote chunk fetching
    fallbackStore?.addFallback(webrtc);

    // Initialize media handler with the tree
    initMediaHandler(tree);

    // Initialize WebRTC signaling with the controller
    initWebRTCSignaling(webrtc);

    // Subscribe to WebRTC signaling events (kind 25050)
    setupWebRTCSignalingSubscription(cfg.pubkey);

    // WebRTC starts when pool config is received (waits for settings to load)
    console.log('[Worker] WebRTC controller ready (waiting for pool config)');

    // Initialize SocialGraph with user's pubkey as root
    socialGraph = new SocialGraph(cfg.pubkey);
    console.log('[Worker] SocialGraph initialized with root:', cfg.pubkey.slice(0, 16) + '...');

    // Respond ready immediately so page can render
    respond({ type: 'ready' });

    // Load persisted social graph from IndexedDB, THEN set up subscriptions
    // This ensures we only subscribe for users whose follow lists we don't already have
    const loaded = await loadSocialGraph(cfg.pubkey);
    if (loaded) {
      notifySocialGraphVersionUpdate();
      console.log('[Worker] Social graph loaded, setting up subscriptions with existing data');
    }
    // Subscribe to kind:3 (contact list) events for social graph
    // Do this AFTER load so we can skip users we already have
    setupSocialGraphSubscription(cfg.pubkey);
  } catch (err) {
    const error = getErrorMessage(err);
    respond({ type: 'error', id, error });
  }
}

// NOTE: WebRTC cannot run in workers - RTCPeerConnection is not available
// See: https://github.com/w3c/webrtc-extensions/issues/77
// WebRTC must run in main thread and proxy to worker for storage

/**
 * Handle identity change (account switch)
 */
function handleSetIdentity(id: string, pubkey: string, nsec?: string) {
  setIdentity(pubkey, nsec);
  console.log('[Worker] Identity updated:', pubkey.slice(0, 16) + '...');

  if (_config) {
    _config.pubkey = pubkey;
    _config.nsec = nsec;
  }

  if (webrtc) {
    webrtc.setIdentity(pubkey);
    setupWebRTCSignalingSubscription(pubkey);
  }

  // Update SocialGraph root
  socialGraph.setRoot(pubkey);
  notifySocialGraphVersionUpdate();
  console.log('[Worker] SocialGraph root updated:', pubkey.slice(0, 16) + '...');

  // Reinitialize Blossom with new signer and progress callback
  if (_config?.blossomServers && _config.blossomServers.length > 0) {
    blossomStore = new BlossomStore({
      servers: _config.blossomServers,  // Pass full config with read/write flags
      signer: createBlossomSigner(),
      onUploadProgress: updateBlossomProgress,
    });
  }

  // NOTE: WebRTC not available in workers

  respond({ type: 'void', id });
}

/**
 * Create Blossom signer using current identity
 */
function createBlossomSigner() {
  return async (event: { kind: 24242; created_at: number; content: string; tags: string[][] }) => {
    const signed = await signEvent({
      kind: event.kind,
      created_at: event.created_at,
      content: event.content,
      tags: event.tags,
    });
    return signed;
  };
}

async function handleClose(id: string) {
  // NOTE: WebRTC not available in workers
  // Close NDK connections
  closeNdk();
  // Clear identity
  clearIdentity();
  // Clear caches
  clearMemoryCache();
  store = null;
  tree = null;
  _config = null;
  respond({ type: 'void', id });
}

// ============================================================================
// Store Handlers (low-level)
// ============================================================================

async function handleGet(id: string, hash: Uint8Array) {
  if (!store) {
    respond({ type: 'result', id, error: 'Store not initialized' });
    return;
  }

  const raceForData = (promises: Array<Promise<Uint8Array | null>>): Promise<Uint8Array | null> => {
    if (promises.length === 0) return Promise.resolve(null);
    return new Promise((resolve) => {
      let remaining = promises.length;
      let settled = false;
      for (const promise of promises) {
        promise.then((result) => {
          if (settled) return;
          if (result) {
            settled = true;
            resolve(result);
            return;
          }
          remaining -= 1;
          if (remaining === 0) resolve(null);
        }).catch(() => {
          if (settled) return;
          remaining -= 1;
          if (remaining === 0) resolve(null);
        });
      }
    });
  };

  // 1. Try local store first
  let data = await store.get(hash);

  // 2. If not found locally, race Blossom and WebRTC (when available).
  // Prefer the first non-null response to avoid WebRTC timeouts delaying Blossom.
  if (!data) {
    const fetches: Array<Promise<Uint8Array | null>> = [];
    let webrtcPromise: Promise<Uint8Array | null> | null = null;

    if (webrtc) {
      const connectedPeers = webrtc.getConnectedCount();
      if (connectedPeers > 0) {
        const WEBRTC_TIMEOUT = WEBRTC_REQUEST_TIMEOUT_MS;
        webrtcPromise = webrtc.get(hash);
        void webrtcPromise.catch(() => {});
        const timeoutPromise = new Promise<Uint8Array | null>(resolve => setTimeout(() => resolve(null), WEBRTC_TIMEOUT));
        fetches.push(Promise.race([webrtcPromise, timeoutPromise]));
      }
    }

    if (blossomStore) {
      fetches.push(blossomStore.get(hash).catch(() => null));
    }

    data = await raceForData(fetches);
    if (data) {
      await store.put(hash, data);
    }

    if (!data && webrtcPromise) {
      // WebRTC timed out or lost the race; let it continue in background and cache if it succeeds.
      webrtcPromise.then(async (lateData) => {
        if (lateData && store) {
          await store.put(hash, lateData);
        }
      }).catch(() => {});
    }
  }

  if (data) {
    // Transfer the ArrayBuffer to avoid copying
    respondWithTransfer({ type: 'result', id, data }, [data.buffer]);
  } else {
    respond({ type: 'result', id, data: undefined });
  }
}

async function handlePut(id: string, hash: Uint8Array, data: Uint8Array) {
  if (!store) {
    respond({ type: 'bool', id, value: false, error: 'Store not initialized' });
    return;
  }

  const success = await store.put(hash, data);
  respond({ type: 'bool', id, value: success });

  // Trigger eviction check (debounced) after successful put
  if (success) {
    runEvictionCheck();
  }

  // Fire-and-forget push to blossom (don't await - optimistic upload)
  if (blossomStore && success) {
    blossomStore.put(hash, data).catch((err) => {
      const hashHex = Array.from(hash.slice(0, 8)).map(b => b.toString(16).padStart(2, '0')).join('');
      console.warn(`[Worker] Blossom upload failed for ${hashHex}...:`, err instanceof Error ? err.message : err);
      respond({ type: 'blossomUploadError', hash: hashHex, error: getErrorMessage(err) });
    });
  }
}

async function handleHas(id: string, hash: Uint8Array) {
  if (!store) {
    respond({ type: 'bool', id, value: false, error: 'Store not initialized' });
    return;
  }

  const exists = await store.has(hash);
  respond({ type: 'bool', id, value: exists });
}

async function handleDelete(id: string, hash: Uint8Array) {
  if (!store) {
    respond({ type: 'bool', id, value: false, error: 'Store not initialized' });
    return;
  }

  const success = await store.delete(hash);
  respond({ type: 'bool', id, value: success });
}

// ============================================================================
// Tree Handlers (high-level)
// ============================================================================

async function handleReadFile(id: string, cid: import('../types').CID) {
  if (!tree) {
    respond({ type: 'result', id, error: 'Tree not initialized' });
    return;
  }

  try {
    const data = await tree.readFile(cid);
    if (data) {
      respondWithTransfer({ type: 'result', id, data }, [data.buffer]);
    } else {
      respond({ type: 'result', id, error: 'File not found' });
    }
  } catch (err) {
    respond({ type: 'result', id, error: getErrorMessage(err) });
  }
}

async function handleReadFileRange(
  id: string,
  cid: import('../types').CID,
  start: number,
  end?: number
) {
  if (!tree) {
    respond({ type: 'result', id, error: 'Tree not initialized' });
    return;
  }

  try {
    const data = await tree.readFileRange(cid, start, end);
    if (data) {
      respondWithTransfer({ type: 'result', id, data }, [data.buffer]);
    } else {
      respond({ type: 'result', id, error: 'File not found' });
    }
  } catch (err) {
    respond({ type: 'result', id, error: getErrorMessage(err) });
  }
}

async function handleReadFileStream(id: string, cid: import('../types').CID) {
  if (!tree) {
    respond({ type: 'streamChunk', id, chunk: new Uint8Array(0), done: true });
    return;
  }

  try {
    for await (const chunk of tree.readFileStream(cid)) {
      // Send each chunk, transferring ownership
      respondWithTransfer(
        { type: 'streamChunk', id, chunk, done: false },
        [chunk.buffer]
      );
    }
    // Signal completion
    respond({ type: 'streamChunk', id, chunk: new Uint8Array(0), done: true });
  } catch (err) {
    respond({ type: 'error', id, error: getErrorMessage(err) });
  }
}

async function handleWriteFile(
  id: string,
  parentCid: import('../types').CID | null,
  path: string,
  data: Uint8Array
) {
  if (!tree) {
    respond({ type: 'cid', id, error: 'Tree not initialized' });
    return;
  }

  try {
    // Parse path to get directory path and filename
    const parts = path.split('/').filter(Boolean);
    const fileName = parts.pop();
    if (!fileName) {
      respond({ type: 'cid', id, error: 'Invalid path' });
      return;
    }

    // First, create a file CID from the data
    const fileResult = await tree.putFile(data);
    const fileCid = fileResult.cid;

    // If no parent, just return the file CID (no directory structure)
    if (!parentCid) {
      respond({ type: 'cid', id, cid: fileCid });
      return;
    }

    // Add the file to the parent directory
    const newRootCid = await tree.setEntry(
      parentCid,
      parts,
      fileName,
      fileCid,
      data.length,
      1 // LinkType.File
    );
    respond({ type: 'cid', id, cid: newRootCid });
  } catch (err) {
    respond({ type: 'cid', id, error: getErrorMessage(err) });
  }
}

async function handleDeleteFile(
  id: string,
  parentCid: import('../types').CID,
  path: string
) {
  if (!tree) {
    respond({ type: 'cid', id, error: 'Tree not initialized' });
    return;
  }

  try {
    // Parse path to get directory path and filename
    const parts = path.split('/').filter(Boolean);
    const fileName = parts.pop();
    if (!fileName) {
      respond({ type: 'cid', id, error: 'Invalid path' });
      return;
    }

    const newCid = await tree.removeEntry(parentCid, parts, fileName);
    respond({ type: 'cid', id, cid: newCid });
  } catch (err) {
    respond({ type: 'cid', id, error: getErrorMessage(err) });
  }
}

async function handleListDir(id: string, cidArg: import('../types').CID) {
  if (!tree) {
    respond({ type: 'dirListing', id, error: 'Tree not initialized' });
    return;
  }

  try {
    const entries = await tree.listDirectory(cidArg);

    const dirEntries = entries.map((entry) => ({
      name: entry.name,
      isDir: entry.type === 2, // LinkType.Dir
      size: entry.size,
      cid: entry.cid,
    }));

    respond({ type: 'dirListing', id, entries: dirEntries });
  } catch (err) {
    respond({ type: 'dirListing', id, error: getErrorMessage(err) });
  }
}

async function handleResolveRoot(id: string, npub: string, path?: string) {
  try {
    // Parse path to get tree name (first segment)
    const pathParts = path?.split('/').filter(Boolean) ?? [];
    const treeName = pathParts[0] || 'public'; // Default to 'public' tree

    // Look up in cache
    const cachedCid = await getCachedRoot(npub, treeName);
    if (!cachedCid) {
      // Not in cache - main thread should subscribe via Nostr
      respond({ type: 'cid', id, cid: undefined });
      return;
    }

    const subPath = pathParts.slice(1);
    if (subPath.length === 0) {
      respond({ type: 'cid', id, cid: cachedCid });
      return;
    }

    if (!tree) {
      respond({ type: 'cid', id, error: 'Tree not initialized' });
      return;
    }

    const resolved = await tree.resolvePath(cachedCid, subPath);
    respond({ type: 'cid', id, cid: resolved?.cid });
  } catch (err) {
    respond({ type: 'cid', id, error: getErrorMessage(err) });
  }
}

// ============================================================================
// Nostr Handlers
// ============================================================================

async function handleSubscribe(id: string, filters: import('./protocol').NostrFilter[]) {
  try {
    // Use the request id as the subscription id
    ndkSubscribe(id, filters);
    respond({ type: 'void', id });
  } catch (err) {
    respond({ type: 'void', id, error: getErrorMessage(err) });
  }
}

async function handleUnsubscribe(id: string, subId: string) {
  try {
    ndkUnsubscribe(subId);
    respond({ type: 'void', id });
  } catch (err) {
    respond({ type: 'void', id, error: getErrorMessage(err) });
  }
}

async function handlePublish(id: string, event: SignedEvent) {
  try {
    await ndkPublish(event);
    respond({ type: 'void', id });
  } catch (err) {
    respond({ type: 'void', id, error: getErrorMessage(err) });
  }
}


// ============================================================================
// Stats Handlers
// ============================================================================

async function handleGetPeerStats(id: string) {
  if (!webrtc) {
    respond({ type: 'peerStats', id, stats: [] });
    return;
  }

  const controllerStats = webrtc.getPeerStats();
  const stats = controllerStats.map(s => ({
    peerId: s.peerId,
    pubkey: s.pubkey,
    connected: s.connected,
    pool: s.pool,
    requestsSent: s.requestsSent,
    requestsReceived: s.requestsReceived,
    responsesSent: s.responsesSent,
    responsesReceived: s.responsesReceived,
    bytesSent: s.bytesSent,
    bytesReceived: s.bytesReceived,
  }));
  respond({ type: 'peerStats', id, stats });
}

async function handleGetRelayStats(id: string) {
  try {
    const stats = getNdkRelayStats();
    respond({ type: 'relayStats', id, stats });
  } catch {
    respond({ type: 'relayStats', id, stats: [] });
  }
}

async function handleGetStorageStats(id: string) {
  try {
    if (!store) {
      respond({ type: 'storageStats', id, items: 0, bytes: 0 });
      return;
    }
    const items = await store.count();
    const bytes = await store.totalBytes();
    respond({ type: 'storageStats', id, items, bytes });
  } catch (e) {
    console.error('[Worker] getStorageStats error:', e);
    respond({ type: 'storageStats', id, items: 0, bytes: 0 });
  }
}

// ============================================================================
// SocialGraph Handlers
// ============================================================================

function handleInitSocialGraph(id: string, rootPubkey?: string) {
  try {
    if (rootPubkey) {
      socialGraph = new SocialGraph(rootPubkey);
    }
    const size = socialGraph.size();
    respond({ type: 'socialGraphInit', id, version: socialGraphVersion, size });
  } catch (err) {
    respond({ type: 'socialGraphInit', id, version: 0, size: 0, error: getErrorMessage(err) });
  }
}

function handleSetSocialGraphRoot(id: string, pubkey: string) {
  try {
    socialGraph.setRoot(pubkey);
    // Update followsSet for WebRTC peer classification
    const follows = socialGraph.getFollowedByUser(pubkey);
    followsSet = new Set(follows);
    notifySocialGraphVersionUpdate();
    respond({ type: 'void', id });
  } catch (err) {
    respond({ type: 'void', id, error: getErrorMessage(err) });
  }
}

function handleSocialGraphEvents(id: string, events: SocialGraphNostrEvent[]) {
  try {
    let updated = false;
    for (const event of events) {
      socialGraph.handleEvent(event);
      updated = true;
    }
    if (updated) {
      notifySocialGraphVersionUpdate();
    }
    respond({ type: 'void', id });
  } catch (err) {
    respond({ type: 'void', id, error: getErrorMessage(err) });
  }
}

function handleGetFollowDistance(id: string, pubkey: string) {
  try {
    const distance = socialGraph.getFollowDistance(pubkey);
    respond({ type: 'followDistance', id, distance });
  } catch (err) {
    respond({ type: 'followDistance', id, distance: 1000, error: getErrorMessage(err) });
  }
}

function handleIsFollowing(id: string, follower: string, followed: string) {
  try {
    const result = socialGraph.isFollowing(follower, followed);
    respond({ type: 'isFollowingResult', id, result });
  } catch (err) {
    respond({ type: 'isFollowingResult', id, result: false, error: getErrorMessage(err) });
  }
}

function handleGetFollowsList(id: string, pubkey: string) {
  try {
    const follows = socialGraph.getFollowedByUser(pubkey);
    respond({ type: 'pubkeyList', id, pubkeys: Array.from(follows) });
  } catch (err) {
    respond({ type: 'pubkeyList', id, pubkeys: [], error: getErrorMessage(err) });
  }
}

function handleGetFollowers(id: string, pubkey: string) {
  try {
    const followers = socialGraph.getFollowersByUser(pubkey);
    respond({ type: 'pubkeyList', id, pubkeys: Array.from(followers) });
  } catch (err) {
    respond({ type: 'pubkeyList', id, pubkeys: [], error: getErrorMessage(err) });
  }
}

function handleGetFollowedByFriends(id: string, pubkey: string) {
  try {
    const friendsFollowing = socialGraph.getFollowedByFriends(pubkey);
    respond({ type: 'pubkeyList', id, pubkeys: Array.from(friendsFollowing) });
  } catch (err) {
    respond({ type: 'pubkeyList', id, pubkeys: [], error: getErrorMessage(err) });
  }
}

function handleGetSocialGraphSize(id: string) {
  try {
    const size = socialGraph.size();
    respond({ type: 'socialGraphSize', id, size });
  } catch (err) {
    respond({ type: 'socialGraphSize', id, size: 0, error: getErrorMessage(err) });
  }
}

/**
 * Fetch a user's follow list when visiting their profile.
 * Only fetches if we don't already have their follow list.
 */
function handleFetchUserFollows(id: string, pubkey: string) {
  try {
    if (!pubkey || pubkey.length !== 64) {
      respond({ type: 'ack', id });
      return;
    }

    // Check if we already have their follow list
    const existingCreatedAt = socialGraph.getFollowListCreatedAt(pubkey);
    if (existingCreatedAt !== undefined) {
      respond({ type: 'ack', id });
      return;
    }

    // Subscribe to their kind:3 event
    if (!subscribedPubkeys.has(pubkey)) {
      subscribedPubkeys.add(pubkey);
      ndkSubscribe(`socialgraph-profile-${pubkey.slice(0, 8)}`, [{
        kinds: [KIND_CONTACTS],
        authors: [pubkey],
      }]);
    }

    respond({ type: 'ack', id });
  } catch (err) {
    respond({ type: 'ack', id, error: getErrorMessage(err) });
  }
}

// Track which pubkeys we've fetched followers for
const fetchedFollowersPubkeys = new Set<string>();

/**
 * Fetch users who follow a given pubkey (for profile views)
 * Subscribes to recent kind:3 events with #p tag mentioning this user
 */
function handleFetchUserFollowers(id: string, pubkey: string) {
  try {
    if (!pubkey || pubkey.length !== 64) {
      respond({ type: 'ack', id });
      return;
    }

    // Only fetch once per pubkey per session
    if (fetchedFollowersPubkeys.has(pubkey)) {
      respond({ type: 'ack', id });
      return;
    }

    // Check if we already have some followers data (>10 means we likely have enough)
    const existingFollowers = socialGraph.followerCount(pubkey);
    if (existingFollowers > 10) {
      respond({ type: 'ack', id });
      return;
    }

    fetchedFollowersPubkeys.add(pubkey);

    // Subscribe to recent kind:3 events that mention this user
    // Limit to 100 to avoid overwhelming the connection
    ndkSubscribe(`socialgraph-followers-${pubkey.slice(0, 8)}`, [{
      kinds: [KIND_CONTACTS],
      '#p': [pubkey],
      limit: 100,
    }]);

    respond({ type: 'ack', id });
  } catch (err) {
    respond({ type: 'ack', id, error: getErrorMessage(err) });
  }
}

// ============================================================================
// SocialGraph Subscription
// ============================================================================

// Track latest event per pubkey to avoid processing old events (for social graph)
// Limited to 1000 entries to prevent memory leak from encountering many unique pubkeys
const socialGraphLatestByPubkey = new LRUCache<string, number>(1000);

// Track which pubkeys we've already subscribed to for follow lists
const subscribedPubkeys = new Set<string>();

// Maximum total users in social graph before stopping crawl
const MAX_SOCIAL_GRAPH_SIZE = 10000;

/**
 * Handle incoming SocialGraph event (kind:3)
 */
function handleSocialGraphEvent(event: SignedEvent): void {
  const rootPubkey = socialGraph.getRoot();

  const prevTime = socialGraphLatestByPubkey.get(event.pubkey) || 0;
  if (event.created_at > prevTime) {
    socialGraphLatestByPubkey.set(event.pubkey, event.created_at);

    // allowUnknownAuthors=true lets us track followers of any user, not just those connected to root
    socialGraph.handleEvent(event as SocialGraphNostrEvent, true);

    // If this is the root user's contact list, update followsSet for WebRTC
    // and subscribe to kind:3 from their follows
    if (event.pubkey === rootPubkey) {
      try {
        const follows = socialGraph.getFollowedByUser(rootPubkey);
        followsSet = new Set(follows);
        console.log('[Worker] Follows updated:', followsSet.size, 'pubkeys');

        // Subscribe to kind:3 from root's follows (depth 1)
        subscribeToFollowsContactLists(Array.from(follows), 1);

        // If user has few follows, also use bootstrap user's follows
        if (follows.size < 5 && rootPubkey !== DEFAULT_BOOTSTRAP_PUBKEY) {
          const bootstrapFollows = socialGraph.getFollowedByUser(DEFAULT_BOOTSTRAP_PUBKEY);
          if (bootstrapFollows.size > 0) {
            subscribeToFollowsContactLists(Array.from(bootstrapFollows), 1);
          }
        }
      } catch (err) {
        console.warn('[Worker] Error getting follows for root:', err);
      }

      // Broadcast hello so peers can re-classify with updated follows
      webrtc?.broadcastHello();
    } else {
      // For non-root users at depth 1, subscribe to their follows (depth 2)
      // But only if graph isn't already at max size
      if (socialGraph.size() < MAX_SOCIAL_GRAPH_SIZE) {
        try {
          const distance = socialGraph.getFollowDistance(event.pubkey);
          if (distance === 1) {
            const theirFollows = socialGraph.getFollowedByUser(event.pubkey);
            subscribeToFollowsContactLists(Array.from(theirFollows), 2);
          }
        } catch (err) {
          console.warn('[Worker] Error getting follows for user:', event.pubkey.slice(0, 16), err);
        }
      }
    }

    notifySocialGraphVersionUpdate();
  }
}

/**
 * Subscribe to kind:3 contact lists from a set of pubkeys
 * Only subscribes for users whose follow list we don't already have (getFollowedByUser returns empty)
 */
function subscribeToFollowsContactLists(pubkeys: string[], depth: number): void {
  // Don't subscribe while still loading from IndexedDB
  if (socialGraphLoading) {
    return;
  }

  // Stop crawling if graph is already large enough
  const currentSize = socialGraph.size();
  if (currentSize >= MAX_SOCIAL_GRAPH_SIZE) {
    return;
  }

  const remainingCapacity = MAX_SOCIAL_GRAPH_SIZE - currentSize;
  const newPubkeys: string[] = [];

  for (const pk of pubkeys) {
    if (subscribedPubkeys.has(pk)) continue;
    if (newPubkeys.length >= remainingCapacity) break;

    // Only fetch if we don't have their follow list (like iris-client does)
    try {
      const existingFollows = socialGraph.getFollowedByUser(pk);
      if (existingFollows.size > 0) {
        subscribedPubkeys.add(pk); // Mark as "done" so we don't check again
        continue;
      }
    } catch {
      // Invalid pubkey in graph, skip
      continue;
    }

    subscribedPubkeys.add(pk);
    newPubkeys.push(pk);
  }

  if (newPubkeys.length > 0) {
    console.log(`[Worker] Subscribing to kind:3 from ${newPubkeys.length} missing pubkeys at depth ${depth} (graph size: ${currentSize})`);
    // Subscribe in batches to avoid overwhelming relays
    const batchSize = 50;
    for (let i = 0; i < newPubkeys.length; i += batchSize) {
      const batch = newPubkeys.slice(i, i + batchSize);
      ndkSubscribe(`socialgraph-depth${depth}-${i}`, [{
        kinds: [KIND_CONTACTS],
        authors: batch,
      }]);
    }
  }
}

/**
 * Subscribe to kind:3 contact list events for social graph
 */
function setupSocialGraphSubscription(rootPubkey: string): void {
  if (!rootPubkey || rootPubkey.length !== 64) {
    console.warn('[Worker] Invalid pubkey for social graph subscription:', rootPubkey);
    return;
  }

  // Clear previous subscriptions tracking when root changes
  subscribedPubkeys.clear();
  subscribedPubkeys.add(rootPubkey);

  // NOTE: Don't call setOnEvent here - use the unified handler set up in handleInit

  // Check if we already have the root user's follows
  const rootFollows = socialGraph.getFollowedByUser(rootPubkey);
  const hasRootFollows = rootFollows.size > 0;

  // Subscribe to root user's contact list if we don't have it
  if (!hasRootFollows) {
    const authors = [rootPubkey];
    if (rootPubkey !== DEFAULT_BOOTSTRAP_PUBKEY) {
      authors.push(DEFAULT_BOOTSTRAP_PUBKEY);
      subscribedPubkeys.add(DEFAULT_BOOTSTRAP_PUBKEY);
    }
    ndkSubscribe('socialgraph-contacts', [{
      kinds: [KIND_CONTACTS],
      authors,
    }]);
  }

  console.log('[Worker] Subscribed to kind:3 events for social graph');
}
