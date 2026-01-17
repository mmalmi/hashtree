/**
 * Toast notification store
 * Manages toast messages with auto-dismiss
 */
import { writable } from 'svelte/store';

export type ToastType = 'info' | 'success' | 'error' | 'warning';

export interface Toast {
  id: string;
  type: ToastType;
  message: string;
  duration?: number; // ms, undefined = no auto-dismiss
}

// Module-level state
let nextId = 1;

// Svelte store for toasts
export const toasts = writable<Toast[]>([]);

export function showToast(type: ToastType, message: string, duration = 4000): string {
  const id = String(nextId++);
  const toast: Toast = { id, type, message, duration };

  toasts.update(t => [...t, toast]);

  if (duration > 0) {
    setTimeout(() => dismissToast(id), duration);
  }

  return id;
}

export function dismissToast(id: string) {
  toasts.update(t => t.filter(toast => toast.id !== id));
}

// Convenience functions
export const toast = {
  info: (message: string, duration?: number) => showToast('info', message, duration),
  success: (message: string, duration?: number) => showToast('success', message, duration),
  error: (message: string, duration?: number) => showToast('error', message, duration ?? 6000),
  warning: (message: string, duration?: number) => showToast('warning', message, duration),
};
