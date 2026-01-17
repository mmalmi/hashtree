/**
 * Global type declarations for window properties
 * Used for HMR singleton patterns and test helpers
 */

import type { NostrStore } from './nostr';
import type { LocalStore } from './store';
import type { SocialGraph } from './utils/socialGraph';
import type { TreeEntry } from 'hashtree';

declare global {
  interface Window {
    // HMR singleton patterns
    __nostrStore?: NostrStore;
    __ndk?: unknown;

    // Test helpers
    __testHelpers?: {
      uploadSingleFile: unknown;
      followPubkey: unknown;
    };
    __testHelpersReady?: boolean;
    __localStore?: LocalStore;
    __getSocialGraph?: () => SocialGraph;
    __socialGraph?: SocialGraph;
    __settingsStore?: unknown;
    __thumbnailCaptureReset?: () => void;
    __testSetDirectoryEntries?: (entries: TreeEntry[]) => void;

    // File System Access API (not in standard TS types)
    showSaveFilePicker?: (options?: {
      suggestedName?: string;
      types?: Array<{
        description?: string;
        accept?: Record<string, string[]>;
      }>;
    }) => Promise<FileSystemFileHandle>;
  }
}

export {};
