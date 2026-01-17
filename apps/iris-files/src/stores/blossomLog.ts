/**
 * Blossom operation log store
 */
import { writable, get } from 'svelte/store';
import type { BlossomLogEntry } from 'hashtree';

const MAX_LOG_ENTRIES = 50;

function createBlossomLogStore() {
  const { subscribe, update } = writable<BlossomLogEntry[]>([]);

  return {
    subscribe,

    add: (entry: BlossomLogEntry) => {
      update(logs => {
        const newLogs = [entry, ...logs];
        // Keep only last N entries
        return newLogs.slice(0, MAX_LOG_ENTRIES);
      });
    },

    clear: () => {
      update(() => []);
    },

    getAll: (): BlossomLogEntry[] => get(blossomLogStore),
  };
}

export const blossomLogStore = createBlossomLogStore();
