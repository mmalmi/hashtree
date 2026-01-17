/**
 * Cashu Wallet Store
 * Wraps coco-cashu Manager with Svelte reactivity
 */
import { writable, get } from 'svelte/store';
import { hkdf } from '@noble/hashes/hkdf.js';
import { sha256 } from '@noble/hashes/sha2.js';
import { Manager } from 'coco-cashu-core';
import { IndexedDbRepositories } from 'coco-cashu-indexeddb';

export const DEFAULT_MINT = 'https://mint.coinos.io';

export interface MintInfo {
  url: string;
  balance: number;
  name?: string;
}

export interface Transaction {
  id: string;
  type: 'receive' | 'send' | 'melt';
  amount: number;
  mintUrl: string;
  createdAt: number;
  status: 'pending' | 'completed' | 'failed';
}

export interface WalletState {
  initialized: boolean;
  balance: number;
  mints: MintInfo[];
  history: Transaction[];
  error: string | null;
  loading: boolean;
}

const initialState: WalletState = {
  initialized: false,
  balance: 0,
  mints: [],
  history: [],
  error: null,
  loading: false,
};

// Internal state
let manager: Manager | null = null;
let cleanupFns: (() => void)[] = [];

// Create writable store
const { subscribe, update, set } = writable<WalletState>(initialState);

/**
 * Derive a 64-byte wallet seed from 32-byte Nostr secret key
 */
function deriveSeed(nostrSecretKey: Uint8Array): Uint8Array {
  // Use HKDF to expand 32-byte key to 64-byte seed
  // salt: "iris-cashu-wallet"
  // info: "wallet-seed"
  const encoder = new TextEncoder();
  const salt = encoder.encode('iris-cashu-wallet');
  const info = encoder.encode('wallet-seed');
  return hkdf(sha256, nostrSecretKey, salt, info, 64);
}

/**
 * Update balance from all mints
 */
async function updateBalance(): Promise<void> {
  if (!manager) return;
  try {
    const balances = await manager.wallet.getBalances();
    const mints: MintInfo[] = [];
    let total = 0;

    for (const [url, balance] of Object.entries(balances)) {
      mints.push({ url, balance });
      total += balance;
    }

    update(s => ({ ...s, balance: total, mints }));
  } catch (e) {
    console.error('[wallet] Failed to update balance:', e);
  }
}

/**
 * Initialize the wallet with Nostr secret key
 */
export async function initWallet(nostrSecretKey: Uint8Array): Promise<void> {
  if (manager) {
    console.log('[wallet] Already initialized');
    return;
  }

  update(s => ({ ...s, loading: true, error: null }));

  try {
    const walletSeed = deriveSeed(nostrSecretKey);

    // Debug: ensure seed is Uint8Array
    if (!(walletSeed instanceof Uint8Array)) {
      throw new Error(`Seed derivation failed: expected Uint8Array, got ${typeof walletSeed}`);
    }
    if (walletSeed.length !== 64) {
      throw new Error(`Seed has wrong length: expected 64, got ${walletSeed.length}`);
    }
    console.log('[wallet] Seed derived successfully, length:', walletSeed.length);

    console.log('[wallet] Creating IndexedDB repositories...');
    const repo = new IndexedDbRepositories({ name: 'iris-wallet' });
    await repo.init();

    console.log('[wallet] Creating Manager...');
    const seedGetter = async () => {
      console.log('[wallet] seedGetter called');
      const seed = new Uint8Array(walletSeed);
      console.log('[wallet] Returning seed, type:', seed.constructor.name, 'length:', seed.length);
      return seed;
    };

    // Test that seedGetter works before passing to Manager
    const testSeed = await seedGetter();
    console.log('[wallet] Test seed call succeeded, length:', testSeed.length);

    // Use Manager constructor directly (as per README)
    try {
      manager = new Manager(repo, seedGetter);
      console.log('[wallet] Manager created successfully');
    } catch (e) {
      console.error('[wallet] Manager creation failed:', e);
      throw e;
    }

    // Enable watchers for automatic quote redemption
    await manager.enableMintQuoteWatcher({ watchExistingPendingOnStart: true });
    await manager.enableProofStateWatcher();

    // Enable processor to automatically redeem paid quotes
    await manager.enableMintQuoteProcessor();
    await manager.quotes.requeuePaidMintQuotes();
    console.log('[wallet] Quote watchers enabled');

    // Subscribe to balance changes
    const unsub1 = manager.on('proof:added', updateBalance);
    const unsub2 = manager.on('proof:spent', updateBalance);
    cleanupFns.push(unsub1, unsub2);

    // Initial balance fetch
    await updateBalance();

    update(s => ({ ...s, initialized: true, loading: false }));
    console.log('[wallet] Initialized');
  } catch (e) {
    console.error('[wallet] Initialization failed:', e);
    update(s => ({
      ...s,
      error: e instanceof Error ? e.message : 'Failed to initialize wallet',
      loading: false,
    }));
  }
}

/**
 * Dispose the wallet
 */
export async function disposeWallet(): Promise<void> {
  if (manager) {
    cleanupFns.forEach(fn => fn());
    cleanupFns = [];
    await manager.dispose();
    manager = null;
    set(initialState);
    console.log('[wallet] Disposed');
  }
}

/**
 * Get the manager instance (for advanced use)
 */
export function getManager(): Manager | null {
  return manager;
}

/**
 * Add a new mint
 */
export async function addMint(mintUrl: string, trusted = true): Promise<void> {
  if (!manager) throw new Error('Wallet not initialized');

  update(s => ({ ...s, loading: true, error: null }));
  try {
    await manager.mint.addMint(mintUrl, { trusted });
    await updateBalance();
    update(s => ({ ...s, loading: false }));
  } catch (e) {
    console.error('[wallet] Failed to add mint:', e);
    update(s => ({
      ...s,
      error: e instanceof Error ? e.message : 'Failed to add mint',
      loading: false,
    }));
    throw e;
  }
}

/**
 * Create a mint quote to receive tokens
 * Returns the lightning invoice to pay
 */
export async function createReceiveQuote(
  amountSats: number,
  mintUrl: string = DEFAULT_MINT
): Promise<{ invoice: string; quoteId: string }> {
  if (!manager) throw new Error('Wallet not initialized');

  if (import.meta.env.VITE_TEST_MODE) {
    return {
      invoice: `lnbc${amountSats}n1${Math.random().toString(36).slice(2)}`,
      quoteId: `test-${Date.now()}`,
    };
  }

  // Check if mint is already added and trusted
  const existingMints = await manager.mint.getAllMints();
  const mintExists = existingMints.some(m => m.url === mintUrl);

  if (!mintExists) {
    await manager.mint.addMint(mintUrl, { trusted: true });
  } else {
    // Ensure mint is trusted
    const isTrusted = await manager.mint.isTrustedMint(mintUrl);
    if (!isTrusted) {
      await manager.mint.trustMint(mintUrl);
    }
  }

  const quote = await manager.quotes.createMintQuote(mintUrl, amountSats);
  return { invoice: quote.request, quoteId: quote.quote };
}

/**
 * Receive ecash token
 */
export async function receiveToken(token: string): Promise<void> {
  if (!manager) throw new Error('Wallet not initialized');

  update(s => ({ ...s, loading: true, error: null }));
  try {
    await manager.wallet.receive(token);
    await updateBalance();
    update(s => ({ ...s, loading: false }));
  } catch (e) {
    console.error('[wallet] Failed to receive token:', e);
    update(s => ({
      ...s,
      error: e instanceof Error ? e.message : 'Failed to receive token',
      loading: false,
    }));
    throw e;
  }
}

/**
 * Pay a lightning invoice from wallet balance
 * Returns success status and any error message
 */
export async function payInvoice(bolt11: string): Promise<{
  success: boolean;
  error?: string;
}> {
  if (!manager) {
    return { success: false, error: 'Wallet not initialized' };
  }

  const state = get(walletStore);
  if (state.balance === 0) {
    return { success: false, error: 'No balance' };
  }

  update(s => ({ ...s, loading: true, error: null }));

  try {
    // Find a mint with balance
    const mintWithBalance = state.mints.find(m => m.balance > 0);
    if (!mintWithBalance) {
      update(s => ({ ...s, loading: false }));
      return { success: false, error: 'No mint with balance' };
    }

    // Create melt quote
    const quote = await manager.quotes.createMeltQuote(mintWithBalance.url, bolt11);

    // Check if we have enough balance (amount + fee)
    const totalNeeded = quote.amount + quote.fee_reserve;
    if (mintWithBalance.balance < totalNeeded) {
      update(s => ({ ...s, loading: false }));
      return { success: false, error: `Insufficient balance. Need ${totalNeeded} sats, have ${mintWithBalance.balance}` };
    }

    // Pay the invoice
    await manager.quotes.payMeltQuote(mintWithBalance.url, quote.quote);
    await updateBalance();
    update(s => ({ ...s, loading: false }));

    return { success: true };
  } catch (e) {
    console.error('[wallet] Payment failed:', e);
    const errorMsg = e instanceof Error ? e.message : 'Payment failed';
    update(s => ({ ...s, error: errorMsg, loading: false }));
    return { success: false, error: errorMsg };
  }
}

/**
 * Get total balance synchronously from store
 */
export function getBalance(): number {
  return get(walletStore).balance;
}

/**
 * Check if wallet is initialized
 */
export function isInitialized(): boolean {
  return get(walletStore).initialized;
}

// Export the store
export const walletStore = { subscribe };
