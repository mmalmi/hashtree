/**
 * Nostr Store - Svelte store for Nostr state
 */
import { writable, get } from 'svelte/store';
import type { TreeVisibility } from 'hashtree';
import { DEFAULT_NETWORK_SETTINGS } from '../stores/settings';

export interface HashTreeEvent {
  id: string;
  pubkey: string;
  name: string;
  /** Root hash (hex encoded) */
  rootHash: string;
  /** Decryption key for encrypted trees (hex encoded) - present for public trees */
  rootKey?: string;
  /** Encrypted key (hex) - present for link-visible trees, decrypt with link key */
  encryptedKey?: string;
  /** Key ID for link-visible trees - hash of link decryption key, allows key rotation */
  keyId?: string;
  /** Self-encrypted key (NIP-44) - present for private and link-visible trees */
  selfEncryptedKey?: string;
  /** Self-encrypted link key (NIP-44) - present for link-visible trees, allows owner to recover link key for sharing */
  selfEncryptedLinkKey?: string;
  /** Computed visibility based on which tags are present */
  visibility: TreeVisibility;
  created_at: number;
}

export type RelayStatus = 'disconnected' | 'connecting' | 'connected' | 'error';

export interface RelayInfo {
  url: string;
  status: RelayStatus;
}

export interface NostrState {
  pubkey: string | null;
  npub: string | null;
  isLoggedIn: boolean;
  selectedTree: HashTreeEvent | null;
  relays: string[];
  relayStatuses: Map<string, RelayStatus>;
  connectedRelays: number;
  /** Relays discovered by NDK (outbox model, etc) that aren't in configured list */
  discoveredRelays: RelayInfo[];
}

function createNostrStore() {
  const defaultRelays = DEFAULT_NETWORK_SETTINGS.relays;
  const { subscribe, update } = writable<NostrState>({
    pubkey: null,
    npub: null,
    isLoggedIn: false,
    selectedTree: null,
    relays: defaultRelays,
    relayStatuses: new Map(defaultRelays.map(url => [url, 'disconnected' as RelayStatus])),
    connectedRelays: 0,
    discoveredRelays: [],
  });

  const store = {
    subscribe,

    setPubkey: (pk: string | null) => {
      update(state => ({ ...state, pubkey: pk }));
    },

    setNpub: (npub: string | null) => {
      update(state => ({ ...state, npub }));
    },

    setIsLoggedIn: (loggedIn: boolean) => {
      update(state => ({ ...state, isLoggedIn: loggedIn }));
    },

    setSelectedTree: (tree: HashTreeEvent | null) => {
      update(state => ({ ...state, selectedTree: tree }));
    },

    setRelays: (relays: string[]) => {
      update(state => ({ ...state, relays }));
    },

    setConnectedRelays: (count: number) => {
      update(state => ({ ...state, connectedRelays: count }));
    },

    setRelayStatus: (url: string, status: RelayStatus) => {
      update(state => {
        const newStatuses = new Map(state.relayStatuses);
        newStatuses.set(url, status);
        return { ...state, relayStatuses: newStatuses };
      });
    },

    setRelayStatuses: (statuses: Map<string, RelayStatus>) => {
      update(state => ({ ...state, relayStatuses: statuses }));
    },

    setDiscoveredRelays: (relays: RelayInfo[]) => {
      update(state => ({ ...state, discoveredRelays: relays }));
    },

    getState: (): NostrState => get(store),

    setState: (newState: Partial<NostrState>) => {
      update(state => ({ ...state, ...newState }));
    },
  };

  return store;
}

// Use existing store from window if available (ensures singleton even with HMR/dynamic imports)
const existingStore = typeof window !== 'undefined' ? (window as Window & { __nostrStore?: ReturnType<typeof createNostrStore> }).__nostrStore : null;

export const nostrStore = existingStore || createNostrStore();

// Expose singleton on window immediately
if (typeof window !== 'undefined') {
  (window as Window & { __nostrStore?: ReturnType<typeof createNostrStore> }).__nostrStore = nostrStore;
}

// Legacy compatibility alias
export const useNostrStore = nostrStore;
