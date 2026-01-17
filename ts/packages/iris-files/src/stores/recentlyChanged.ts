/**
 * Store for managing recently changed files (for pulse animation)
 * Uses Svelte stores
 */
import { writable } from 'svelte/store';

// Svelte store for recently changed files
export const recentlyChangedFiles = writable<Set<string>>(new Set());

// Per-file timers - when a file is re-marked, we cancel the old timer
const fileTimers = new Map<string, ReturnType<typeof setTimeout>>();

// Clear a single file after delay
function clearFileAfterDelay(fileName: string, delayMs: number) {
  // Cancel existing timer for this file if any
  const existingTimer = fileTimers.get(fileName);
  if (existingTimer) {
    clearTimeout(existingTimer);
  }

  // Set new timer
  const timer = setTimeout(() => {
    fileTimers.delete(fileName);
    recentlyChangedFiles.update(current => {
      if (current.has(fileName)) {
        const remaining = new Set([...current].filter(f => f !== fileName));
        return remaining;
      }
      return current;
    });
  }, delayMs);

  fileTimers.set(fileName, timer);
}

/**
 * Mark files as recently changed (triggers pulse animation in FileBrowser)
 * Files are automatically cleared after 5 seconds
 * If a file is marked again before timeout, the timer resets
 */
export function markFilesChanged(fileNames: Set<string>) {
  recentlyChangedFiles.update(current => new Set([...current, ...fileNames]));

  // Set individual timers for each file (resets if already set)
  for (const fileName of fileNames) {
    clearFileAfterDelay(fileName, 5000);
  }
}
