/**
 * Tauri integration utilities
 * Provides type-safe wrappers for Tauri APIs with graceful fallbacks when running in browser
 */

import { logHtreeDebug } from './lib/htreeDebug';

export function hasTauriInvoke(): boolean {
  if (typeof window === 'undefined') return false;
  const w = window as unknown as {
    __TAURI_INTERNALS__?: { invoke?: unknown };
    __TAURI__?: { invoke?: unknown; core?: { invoke?: unknown } };
  };
  return typeof w.__TAURI_INTERNALS__?.invoke === 'function'
    || typeof w.__TAURI__?.core?.invoke === 'function'
    || typeof w.__TAURI__?.invoke === 'function';
}

// Check if running in Tauri (desktop app)
let tauriCheckLogged = false;
export const isTauri = (): boolean => {
  if (typeof window === 'undefined') return false;
  const hasTauriGlobals =
    '__TAURI_INTERNALS__' in window ||
    '__TAURI__' in window ||
    '__TAURI_IPC__' in window ||
    '__TAURI_METADATA__' in window;
  const hasInvoke = hasTauriInvoke();

  // Tauri webviews always use the tauri:// protocol even if globals are hidden.
  const protocol = window.location?.protocol || '';
  const normalizedProtocol = protocol.endsWith(':') ? protocol.slice(0, -1) : protocol;
  const href = window.location?.href || '';
  const hostname = window.location?.hostname || '';
  const isTauriHost = hostname === 'tauri.localhost' || hostname.endsWith('.tauri.localhost');
  const userAgent = navigator.userAgent || '';
  const result =
    (hasTauriGlobals && hasInvoke) ||
    normalizedProtocol === 'tauri' ||
    normalizedProtocol === 'asset' ||
    href.startsWith('tauri://') ||
    isTauriHost ||
    userAgent.includes('Tauri');

  if (!tauriCheckLogged) {
    tauriCheckLogged = true;
    logHtreeDebug('tauri:detect', {
      hasTauriGlobals,
      hasInvoke,
      protocol,
      href,
      hostname,
      isTauriHost,
      userAgent,
      result,
    });
  }
  return result;
};

// Check if running on macOS (sync check using navigator)
export const isMacOS = (): boolean => {
  if (typeof navigator === 'undefined') return false;
  return navigator.platform?.toLowerCase().includes('mac') ||
    navigator.userAgent?.toLowerCase().includes('mac');
};

// Check if running on Linux (sync check using navigator)
export const isLinux = (): boolean => {
  if (typeof navigator === 'undefined') return false;
  return navigator.platform?.toLowerCase().includes('linux') ||
    navigator.userAgent?.toLowerCase().includes('linux');
};

// Autostart management
export interface AutostartAPI {
  isEnabled: () => Promise<boolean>;
  enable: () => Promise<void>;
  disable: () => Promise<void>;
}

let autostartAPI: AutostartAPI | null = null;

async function getAutostartAPI(): Promise<AutostartAPI | null> {
  if (!isTauri()) return null;
  if (autostartAPI) return autostartAPI;

  try {
    const { isEnabled, enable, disable } = await import('@tauri-apps/plugin-autostart');
    autostartAPI = { isEnabled, enable, disable };
    return autostartAPI;
  } catch (e) {
    console.warn('Autostart plugin not available:', e);
    return null;
  }
}

export async function isAutostartEnabled(): Promise<boolean> {
  const api = await getAutostartAPI();
  if (!api) return false;
  try {
    return await api.isEnabled();
  } catch (e) {
    console.error('Failed to check autostart status:', e);
    return false;
  }
}

export async function enableAutostart(): Promise<boolean> {
  const api = await getAutostartAPI();
  if (!api) return false;
  try {
    await api.enable();
    return true;
  } catch (e) {
    console.error('Failed to enable autostart:', e);
    return false;
  }
}

export async function disableAutostart(): Promise<boolean> {
  const api = await getAutostartAPI();
  if (!api) return false;
  try {
    await api.disable();
    return true;
  } catch (e) {
    console.error('Failed to disable autostart:', e);
    return false;
  }
}

export async function toggleAutostart(enabled: boolean): Promise<boolean> {
  if (enabled) {
    return enableAutostart();
  } else {
    return disableAutostart();
  }
}

// OS detection
export interface OSInfo {
  platform: string;
  version: string;
  arch: string;
}

export async function getOSInfo(): Promise<OSInfo | null> {
  if (!isTauri()) return null;

  try {
    const { platform, version, arch } = await import('@tauri-apps/plugin-os');
    return {
      platform: platform(),
      version: version(),
      arch: arch(),
    };
  } catch (e) {
    console.warn('OS plugin not available:', e);
    return null;
  }
}

// Dialog utilities
export async function openFile(options?: {
  multiple?: boolean;
  directory?: boolean;
  filters?: Array<{ name: string; extensions: string[] }>;
}): Promise<string[] | null> {
  if (!isTauri()) return null;

  try {
    const { open } = await import('@tauri-apps/plugin-dialog');
    const result = await open({
      multiple: options?.multiple ?? false,
      directory: options?.directory ?? false,
      filters: options?.filters,
    });

    if (!result) return null;
    return Array.isArray(result) ? result : [result];
  } catch (e) {
    console.error('Failed to open file dialog:', e);
    return null;
  }
}

export async function saveFile(options?: {
  defaultPath?: string;
  filters?: Array<{ name: string; extensions: string[] }>;
}): Promise<string | null> {
  if (!isTauri()) return null;

  try {
    const { save } = await import('@tauri-apps/plugin-dialog');
    return await save({
      defaultPath: options?.defaultPath,
      filters: options?.filters,
    });
  } catch (e) {
    console.error('Failed to open save dialog:', e);
    return null;
  }
}

// External URL opener
export async function openExternal(url: string): Promise<boolean> {
  if (!isTauri()) {
    // Fallback to window.open in browser
    window.open(url, '_blank', 'noopener,noreferrer');
    return true;
  }

  try {
    const { openUrl } = await import('@tauri-apps/plugin-opener');
    await openUrl(url);
    return true;
  } catch (e) {
    console.error('Failed to open external URL:', e);
    // Fallback
    window.open(url, '_blank', 'noopener,noreferrer');
    return false;
  }
}

// Notifications
export async function sendNotification(options: {
  title: string;
  body?: string;
}): Promise<boolean> {
  if (!isTauri()) {
    // Fallback to Web Notification API
    if ('Notification' in window) {
      if (Notification.permission === 'granted') {
        new Notification(options.title, { body: options.body });
        return true;
      } else if (Notification.permission !== 'denied') {
        const permission = await Notification.requestPermission();
        if (permission === 'granted') {
          new Notification(options.title, { body: options.body });
          return true;
        }
      }
    }
    return false;
  }

  try {
    const { sendNotification: tauriNotify } = await import('@tauri-apps/plugin-notification');
    tauriNotify(options);
    return true;
  } catch (e) {
    console.error('Failed to send notification:', e);
    return false;
  }
}
