/**
 * Nostr integration for HashTree Explorer
 * Uses NDK with Dexie cache for IndexedDB persistence
 */

// Re-export TreeVisibility from hashtree lib
export type { TreeVisibility } from 'hashtree';

// Store exports
export {
  nostrStore,
  useNostrStore,
  type NostrState,
  type HashTreeEvent,
  type RelayStatus,
  type RelayInfo,
} from './store';

// NDK exports
export {
  ndk,
  signEvent,
  NDKEvent,
  NDKPrivateKeySigner,
  NDKNip07Signer,
  type NostrEvent,
} from './ndk';

// Relay management exports
export {
  updateConnectedRelayCount,
  initRelayTracking,
  normalizeRelayUrl,
} from './relays';

// Authentication exports
export {
  restoreSession,
  loginWithExtension,
  loginWithNsec,
  generateNewKey,
  initReadonlyWorker,
  logout,
  getSecretKey,
  getNsec,
  encrypt,
  decrypt,
} from './auth';

// Tree management exports
export {
  saveHashtree,
  publishTreeRoot,
  deleteTree,
  autosaveIfOwn,
  isOwnTree,
  parseVisibility,
  pubkeyToNpub,
  npubToPubkey,
  linkKeyUtils,
  type SaveHashtreeOptions,
} from './trees';

// Re-export stopWebRTC for backwards compatibility (actual impl is in ../store)
export { stopWebRTC } from '../store';
