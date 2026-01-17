/**
 * App bookmarks store
 *
 * Stores bookmarked/installed apps in localStorage
 */

import { writable } from 'svelte/store';

export interface AppBookmark {
  url: string;
  name: string;
  icon?: string;
  addedAt: number;
}

const STORAGE_KEY = 'iris:apps';

function loadApps(): AppBookmark[] {
  if (typeof localStorage === 'undefined') return [];
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    return stored ? JSON.parse(stored) : [];
  } catch {
    return [];
  }
}

function saveApps(apps: AppBookmark[]) {
  if (typeof localStorage === 'undefined') return;
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(apps));
  } catch {
    // Ignore storage errors
  }
}

function createAppsStore() {
  const { subscribe, set, update } = writable<AppBookmark[]>(loadApps());

  return {
    subscribe,

    add(app: AppBookmark) {
      update((apps) => {
        // Don't add duplicates
        if (apps.some((a) => a.url === app.url)) {
          return apps;
        }
        const newApps = [...apps, app];
        saveApps(newApps);
        return newApps;
      });
    },

    remove(url: string) {
      update((apps) => {
        const newApps = apps.filter((a) => a.url !== url);
        saveApps(newApps);
        return newApps;
      });
    },

    update(url: string, updates: Partial<AppBookmark>) {
      update((apps) => {
        const newApps = apps.map((a) =>
          a.url === url ? { ...a, ...updates } : a
        );
        saveApps(newApps);
        return newApps;
      });
    },

    clear() {
      set([]);
      saveApps([]);
    },
  };
}

export const appsStore = createAppsStore();
