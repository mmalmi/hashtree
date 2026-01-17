/**
 * Multi-account management for HashTree Explorer
 * Stores multiple accounts and allows switching between them
 * Uses Svelte writable stores instead of Zustand
 */
import { writable, get } from 'svelte/store';
import { nip19, getPublicKey } from 'nostr-tools';

// Storage key for accounts list
const STORAGE_KEY_ACCOUNTS = 'hashtree:accounts';
const STORAGE_KEY_ACTIVE_ACCOUNT = 'hashtree:activeAccount';

export type AccountType = 'nsec' | 'extension';

export interface Account {
  pubkey: string;
  npub: string;
  type: AccountType;
  nsec?: string; // Only for nsec accounts
  addedAt: number;
}

interface AccountsState {
  accounts: Account[];
  activeAccountPubkey: string | null;
}

// Create the Svelte store
function createAccountsStore() {
  const { subscribe, set, update } = writable<AccountsState>({
    accounts: [],
    activeAccountPubkey: null,
  });

  const store = {
    subscribe,

    setAccounts: (accounts: Account[]) => {
      update(state => ({ ...state, accounts }));
    },

    setActiveAccount: (pubkey: string | null) => {
      update(state => ({ ...state, activeAccountPubkey: pubkey }));
    },

    addAccount: (account: Account) => {
      update(state => {
        // Don't add duplicates
        if (state.accounts.some(a => a.pubkey === account.pubkey)) {
          return state;
        }
        const newAccounts = [...state.accounts, account];
        saveAccountsToStorage(newAccounts);
        return { ...state, accounts: newAccounts };
      });
    },

    removeAccount: (pubkey: string): boolean => {
      const state = get(accountsStore);
      // Don't allow removing the last account
      if (state.accounts.length <= 1) {
        return false;
      }

      const newAccounts = state.accounts.filter(a => a.pubkey !== pubkey);
      saveAccountsToStorage(newAccounts);

      let newActiveAccountPubkey = state.activeAccountPubkey;
      // If removing active account, switch to another
      if (state.activeAccountPubkey === pubkey && newAccounts.length > 0) {
        newActiveAccountPubkey = newAccounts[0].pubkey;
        localStorage.setItem(STORAGE_KEY_ACTIVE_ACCOUNT, newAccounts[0].pubkey);
      }

      set({ accounts: newAccounts, activeAccountPubkey: newActiveAccountPubkey });
      return true;
    },

    // Get current state synchronously (for compatibility with Zustand patterns)
    getState: (): AccountsState => get(accountsStore),

    // Set state directly (for compatibility with Zustand patterns)
    setState: (newState: Partial<AccountsState>) => {
      update(state => ({ ...state, ...newState }));
    },
  };

  return store;
}

export const accountsStore = createAccountsStore();

// Legacy compatibility alias (matches Zustand API)
export const useAccountsStore = accountsStore;

/**
 * Save accounts to localStorage (nsec stored for nsec accounts)
 */
function saveAccountsToStorage(accounts: Account[]) {
  const data = accounts.map(a => ({
    pubkey: a.pubkey,
    npub: a.npub,
    type: a.type,
    nsec: a.nsec,
    addedAt: a.addedAt,
  }));
  localStorage.setItem(STORAGE_KEY_ACCOUNTS, JSON.stringify(data));
}

/**
 * Load accounts from localStorage
 */
export function loadAccountsFromStorage(): Account[] {
  try {
    const data = localStorage.getItem(STORAGE_KEY_ACCOUNTS);
    if (!data) return [];
    return JSON.parse(data) as Account[];
  } catch {
    return [];
  }
}

/**
 * Get active account pubkey from localStorage
 */
export function getActiveAccountFromStorage(): string | null {
  return localStorage.getItem(STORAGE_KEY_ACTIVE_ACCOUNT);
}

/**
 * Save active account to localStorage
 */
export function saveActiveAccountToStorage(pubkey: string | null) {
  if (pubkey) {
    localStorage.setItem(STORAGE_KEY_ACTIVE_ACCOUNT, pubkey);
  } else {
    localStorage.removeItem(STORAGE_KEY_ACTIVE_ACCOUNT);
  }
}

/**
 * Create account from nsec
 */
export function createAccountFromNsec(nsec: string): Account | null {
  try {
    const decoded = nip19.decode(nsec);
    if (decoded.type !== 'nsec') return null;
    const secretKey = decoded.data as Uint8Array;
    const pubkey = getPublicKey(secretKey);
    return {
      pubkey,
      npub: nip19.npubEncode(pubkey),
      type: 'nsec',
      nsec,
      addedAt: Date.now(),
    };
  } catch {
    return null;
  }
}

/**
 * Create account for extension (pubkey already known)
 */
export function createExtensionAccount(pubkey: string): Account {
  return {
    pubkey,
    npub: nip19.npubEncode(pubkey),
    type: 'extension',
    addedAt: Date.now(),
  };
}

/**
 * Check if extension account already exists
 */
export function hasExtensionAccount(): boolean {
  const state = accountsStore.getState();
  return state.accounts.some(a => a.type === 'extension');
}

/**
 * Check if window.nostr is available
 */
export function hasNostrExtension(): boolean {
  return typeof window !== 'undefined' && !!window.nostr;
}

/**
 * Initialize accounts store from localStorage
 */
export function initAccountsStore() {
  const accounts = loadAccountsFromStorage();
  const activeAccountPubkey = getActiveAccountFromStorage();

  accountsStore.setState({
    accounts,
    activeAccountPubkey: activeAccountPubkey && accounts.some(a => a.pubkey === activeAccountPubkey)
      ? activeAccountPubkey
      : accounts.length > 0 ? accounts[0].pubkey : null,
  });
}
