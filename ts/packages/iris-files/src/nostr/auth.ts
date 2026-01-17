/**
 * Nostr Authentication and Encryption
 */
import { generateSecretKey, getPublicKey, nip19, nip44 } from 'nostr-tools';
import { ndk, NDKPrivateKeySigner, NDKNip07Signer, NDKEvent } from './ndk';
import { nostrStore } from './store';
import { initHashtreeWorker, getWorkerAdapter, updateFollowsSubscription, waitForWorkerAdapter } from '../lib/workerInit';
import {
  accountsStore,
  initAccountsStore,
  createAccountFromNsec,
  createExtensionAccount,
  saveActiveAccountToStorage,
} from '../accounts';
import { stopWebRTC } from '../store';
import { needsMigrations, runMigrations } from '../migrations';
import { initWallet, disposeWallet } from '../stores/wallet';

// Storage keys
const STORAGE_KEY_NSEC = 'hashtree:nsec';
const STORAGE_KEY_LOGIN_TYPE = 'hashtree:loginType';

// Private key (only set for nsec login)
let secretKey: Uint8Array | null = null;
let bootstrapPubkey: string | null = null;
let bootstrapSecretKey: Uint8Array | null = null;
let bootstrapUsedForLogin = false;
const isTestMode = !!import.meta.env.VITE_TEST_MODE;

/**
 * Get the secret key for decryption (only available for nsec login)
 */
export function getSecretKey(): Uint8Array | null {
  return secretKey;
}

/**
 * Get the nsec string (only available for nsec login)
 */
export function getNsec(): string | null {
  if (!secretKey) return null;
  return nip19.nsecEncode(secretKey);
}

/**
 * Initialize or update worker with user identity.
 */
async function initOrUpdateWorkerIdentity(pubkey: string, nsecHex?: string): Promise<void> {
  const adapter = getWorkerAdapter();
  if (adapter) {
    await adapter.setIdentity(pubkey, nsecHex);
    updateFollowsSubscription(pubkey);
  } else {
    await initHashtreeWorker({ pubkey, nsec: nsecHex });
    const readyAdapter = getWorkerAdapter();
    if (readyAdapter) {
      await readyAdapter.setIdentity(pubkey, nsecHex);
      updateFollowsSubscription(pubkey);
    }
  }
}

/**
 * Initialize the worker early for read-only access.
 * This avoids waiting on login flows before connecting to relays.
 */
export async function initReadonlyWorker(): Promise<void> {
  if (getWorkerAdapter()) return;
  if (secretKey) {
    const pubkey = getPublicKey(secretKey);
    const nsecHex = Array.from(secretKey).map(b => b.toString(16).padStart(2, '0')).join('');
    await initHashtreeWorker({ pubkey, nsec: nsecHex });
    return;
  }
  ensureBootstrapIdentity();
  const nsecHex = bootstrapSecretKey
    ? Array.from(bootstrapSecretKey).map(b => b.toString(16).padStart(2, '0')).join('')
    : undefined;
  await initHashtreeWorker({ pubkey: bootstrapPubkey, nsec: nsecHex });
}

/**
 * Wait for window.nostr to be available
 */
async function waitForNostrExtension(timeoutMs = 2000): Promise<boolean> {
  if (window.nostr) return true;

  return new Promise((resolve) => {
    const startTime = Date.now();
    const checkInterval = setInterval(() => {
      if (window.nostr) {
        clearInterval(checkInterval);
        resolve(true);
      } else if (Date.now() - startTime > timeoutMs) {
        clearInterval(checkInterval);
        resolve(false);
      }
    }, 100);
  });
}

/**
 * Try to restore session from localStorage
 */
export async function restoreSession(): Promise<boolean> {
  const t0 = performance.now();
  const logT = (msg: string) => console.log(`[restoreSession] ${msg}: ${Math.round(performance.now() - t0)}ms`);

  initAccountsStore();
  logT('initAccountsStore');

  // Migrate legacy single account to multi-account storage if needed
  const legacyLoginType = localStorage.getItem(STORAGE_KEY_LOGIN_TYPE);
  const legacyNsec = localStorage.getItem(STORAGE_KEY_NSEC);
  const accountsState = accountsStore.getState();

  if (accountsState.accounts.length === 0 && (legacyLoginType || legacyNsec)) {
    if (legacyLoginType === 'nsec' && legacyNsec) {
      const account = createAccountFromNsec(legacyNsec);
      if (account) {
        accountsStore.addAccount(account);
        accountsStore.setActiveAccount(account.pubkey);
        saveActiveAccountToStorage(account.pubkey);
      }
    }
  }

  const activeAccount = accountsState.accounts.find(
    a => a.pubkey === accountsState.activeAccountPubkey
  );

  if (activeAccount) {
    logT('found activeAccount');
    if (activeAccount.type === 'extension') {
      const result = await loginWithExtension();
      if (result) {
        await ensureTestDefaultFolders();
      }
      return result;
    } else if (activeAccount.type === 'nsec' && activeAccount.nsec) {
      logT('calling loginWithNsec');
      const result = await loginWithNsec(activeAccount.nsec, false);
      logT('loginWithNsec done');
      if (result) {
        await ensureTestDefaultFolders();
      }
      return result;
    }
  }

  if (legacyLoginType === 'extension') {
    const result = await loginWithExtension();
    if (result) {
      await ensureTestDefaultFolders();
    }
    return result;
  } else if (legacyLoginType === 'nsec' && legacyNsec) {
    const result = await loginWithNsec(legacyNsec);
    if (result) {
      await ensureTestDefaultFolders();
    }
    return result;
  }

  logT('calling generateInitialKey');
  await generateInitialKey();
  await ensureTestDefaultFolders();
  logT('generateInitialKey done');
  return true;
}

/**
 * Login with NIP-07 browser extension
 */
export async function loginWithExtension(): Promise<boolean> {
  try {
    const extensionAvailable = await waitForNostrExtension();
    if (!extensionAvailable) {
      throw new Error('No nostr extension found');
    }

    const signer = new NDKNip07Signer();
    ndk.signer = signer;

    const user = await signer.user();
    const pk = user.pubkey;

    nostrStore.setPubkey(pk);
    nostrStore.setNpub(nip19.npubEncode(pk));
    nostrStore.setIsLoggedIn(true);
    secretKey = null;

    localStorage.setItem(STORAGE_KEY_LOGIN_TYPE, 'extension');
    localStorage.removeItem(STORAGE_KEY_NSEC);

    const accountsState = accountsStore.getState();
    if (!accountsState.accounts.some(a => a.pubkey === pk)) {
      const account = createExtensionAccount(pk);
      accountsStore.addAccount(account);
    }
    accountsStore.setActiveAccount(pk);
    saveActiveAccountToStorage(pk);

    await initOrUpdateWorkerIdentity(pk);

    // Run migrations in background (delay to allow relays to connect)
    if (needsMigrations()) {
      const npub = nip19.npubEncode(pk);
      setTimeout(() => runMigrations(npub), 5000);
    }

    return true;
  } catch (e) {
    console.error('Extension login failed:', e);
    return false;
  }
}

/**
 * Login with nsec
 */
export async function loginWithNsec(nsec: string, save = true): Promise<boolean> {
  try {
    const decoded = nip19.decode(nsec);
    if (decoded.type !== 'nsec') {
      throw new Error('Invalid nsec');
    }

    secretKey = decoded.data as Uint8Array;
    const pk = getPublicKey(secretKey);

    const signer = new NDKPrivateKeySigner(nsec);
    ndk.signer = signer;

    nostrStore.setPubkey(pk);
    nostrStore.setNpub(nip19.npubEncode(pk));
    nostrStore.setIsLoggedIn(true);

    if (save) {
      localStorage.setItem(STORAGE_KEY_LOGIN_TYPE, 'nsec');
      localStorage.setItem(STORAGE_KEY_NSEC, nsec);

      const accountsState = accountsStore.getState();
      if (!accountsState.accounts.some(a => a.pubkey === pk)) {
        const account = createAccountFromNsec(nsec);
        if (account) {
          accountsStore.addAccount(account);
        }
      }
      accountsStore.setActiveAccount(pk);
      saveActiveAccountToStorage(pk);
    }

    const nsecHex = Array.from(secretKey).map(b => b.toString(16).padStart(2, '0')).join('');
    await initOrUpdateWorkerIdentity(pk, nsecHex);

    // Initialize wallet with secret key
    initWallet(secretKey).catch(e => {
      console.error('Wallet initialization failed:', e);
    });

    // Run migrations in background (delay to allow relays to connect)
    if (needsMigrations()) {
      const npub = nip19.npubEncode(pk);
      setTimeout(() => runMigrations(npub), 5000);
    }

    return true;
  } catch (e) {
    console.error('Nsec login failed:', e);
    return false;
  }
}

/**
 * Generate new keypair
 */
async function applySecretKey(nextKey: Uint8Array): Promise<{ nsec: string; npub: string }> {
  secretKey = nextKey;
  const pk = getPublicKey(nextKey);
  const nsec = nip19.nsecEncode(nextKey);

  const signer = new NDKPrivateKeySigner(nsec);
  ndk.signer = signer;

  nostrStore.setPubkey(pk);
  const npubStr = nip19.npubEncode(pk);
  nostrStore.setNpub(npubStr);
  nostrStore.setIsLoggedIn(true);

  localStorage.setItem(STORAGE_KEY_LOGIN_TYPE, 'nsec');
  localStorage.setItem(STORAGE_KEY_NSEC, nsec);

  const account = createAccountFromNsec(nsec);
  if (account) {
    accountsStore.addAccount(account);
    accountsStore.setActiveAccount(pk);
    saveActiveAccountToStorage(pk);
  }

  const nsecHex = Array.from(nextKey).map(b => b.toString(16).padStart(2, '0')).join('');
  await initOrUpdateWorkerIdentity(pk, nsecHex);

  // Initialize wallet with secret key
  initWallet(nextKey).catch(e => {
    console.error('Wallet initialization failed:', e);
  });

  // Create default folders for new user
  await createDefaultFolders();

  // Publish initial profile with npub.cash lightning address
  publishInitialProfile(npubStr).catch(e => {
    console.error('Failed to publish initial profile:', e);
  });

  return { nsec, npub: npubStr };
}

async function generateInitialKey(): Promise<{ nsec: string; npub: string }> {
  ensureBootstrapIdentity();
  if (bootstrapSecretKey && !bootstrapUsedForLogin) {
    const nextKey = bootstrapSecretKey;
    bootstrapUsedForLogin = true;
    return applySecretKey(nextKey);
  }
  return applySecretKey(generateSecretKey());
}

export async function generateNewKey(): Promise<{ nsec: string; npub: string }> {
  return applySecretKey(generateSecretKey());
}

/**
 * Create default folders for a new user
 */
async function createDefaultFolders() {
  try {
    const waitMs = isTestMode ? 15000 : 5000;
    let adapter = await waitForWorkerAdapter(waitMs);
    if (!adapter && isTestMode) {
      await new Promise(resolve => setTimeout(resolve, 1000));
      adapter = await waitForWorkerAdapter(10000);
    }
    if (!adapter) {
      throw new Error('Worker not ready');
    }

    const { createTree } = await import('../actions');
    const { getLocalRootCache } = await import('../treeRootCache');
    const state = nostrStore.getState();
    if (!state.npub) return;

    const defaults: Array<{ name: string; visibility: 'public' | 'link-visible' | 'private' }> = [
      { name: 'public', visibility: 'public' },
      { name: 'link', visibility: 'link-visible' },
      { name: 'private', visibility: 'private' },
    ];

    for (const { name, visibility } of defaults) {
      if (getLocalRootCache(state.npub, name)) continue;
      await createTree(name, visibility, true);
    }
  } catch (e) {
    console.error('Failed to create default folders:', e);
  }
}

async function ensureTestDefaultFolders(): Promise<void> {
  if (!isTestMode) return;

  const state = nostrStore.getState();
  if (!state.npub) return;

  const { getLocalRootCache } = await import('../treeRootCache');
  const defaults = ['public', 'link', 'private'];
  if (defaults.some(name => getLocalRootCache(state.npub!, name))) {
    return;
  }

  const { getRefResolver } = await import('../refResolver');
  const resolver = getRefResolver();

  const entries = await new Promise<{ key: string }[]>((resolve) => {
    let resolved = false;
    const unsub = resolver.list?.(state.npub!, (list) => {
      if (resolved) return;
      resolved = true;
      setTimeout(() => unsub?.(), 0);
      resolve(list);
    });

    setTimeout(() => {
      if (!resolved) {
        resolved = true;
        unsub?.();
        resolve([]);
      }
    }, 1500);
  });

  if (entries.length > 0) return;
  await createDefaultFolders();
}

/**
 * Publish initial profile with npub.cash lightning address
 */
async function publishInitialProfile(npub: string) {
  const lud16 = `${npub}@npub.cash`;

  const event = new NDKEvent(ndk);
  event.kind = 0;
  event.content = JSON.stringify({ lud16 });

  await event.publish();
  console.log('[auth] Published initial profile with lud16:', lud16);
}

/**
 * Logout
 */
export function logout() {
  nostrStore.setPubkey(null);
  nostrStore.setNpub(null);
  nostrStore.setIsLoggedIn(false);
  nostrStore.setSelectedTree(null);
  secretKey = null;
  ndk.signer = undefined;

  stopWebRTC();

  // Dispose wallet
  disposeWallet().catch(e => {
    console.error('Wallet disposal failed:', e);
  });

  localStorage.removeItem(STORAGE_KEY_LOGIN_TYPE);
  localStorage.removeItem(STORAGE_KEY_NSEC);
}

// ============================================================================
// NIP-44 Encryption/Decryption
// Works with both nsec login (direct) and extension login (via window.nostr)
// ============================================================================

/**
 * Encrypt plaintext for a recipient using NIP-44
 * Works with both nsec and extension login
 */
export async function encrypt(recipientPubkey: string, plaintext: string): Promise<string> {
  if (secretKey) {
    // Direct encryption with secret key
    const conversationKey = nip44.v2.utils.getConversationKey(secretKey, recipientPubkey);
    return nip44.v2.encrypt(plaintext, conversationKey);
  }

  // Extension encryption
  const nostr = (window as unknown as { nostr?: { nip44?: { encrypt: (pk: string, pt: string) => Promise<string> } } }).nostr;
  if (!nostr?.nip44?.encrypt) {
    throw new Error('NIP-44 encryption not available');
  }
  return nostr.nip44.encrypt(recipientPubkey, plaintext);
}

/**
 * Decrypt ciphertext from a sender using NIP-44
 * Works with both nsec and extension login
 */
export async function decrypt(senderPubkey: string, ciphertext: string): Promise<string> {
  if (secretKey) {
    // Direct decryption with secret key
    const conversationKey = nip44.v2.utils.getConversationKey(secretKey, senderPubkey);
    return nip44.v2.decrypt(ciphertext, conversationKey);
  }

  // Extension decryption
  const nostr = (window as unknown as { nostr?: { nip44?: { decrypt: (pk: string, ct: string) => Promise<string> } } }).nostr;
  if (!nostr?.nip44?.decrypt) {
    throw new Error('NIP-44 decryption not available');
  }
  return nostr.nip44.decrypt(senderPubkey, ciphertext);
}
function ensureBootstrapIdentity(): void {
  if (!bootstrapPubkey || !bootstrapSecretKey) {
    const tempKey = generateSecretKey();
    bootstrapSecretKey = tempKey;
    bootstrapPubkey = getPublicKey(tempKey);
  }
}
