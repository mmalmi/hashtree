import type { BlossomServerConfig } from './protocol.js';
import { getInjectedHtreeServerUrl, type HtreeRuntimeWindowLike } from './runtime.js';

export interface RuntimeNetworkOptions {
  windowLike?: HtreeRuntimeWindowLike;
}

export interface ResolveRuntimeEndpointsOptions extends RuntimeNetworkOptions {
  relays?: readonly string[];
  blossomServers?: readonly BlossomServerConfig[];
}

export interface RuntimeEndpoints {
  htreeServerUrl: string | null;
  nostrRelays: string[];
  blossomServers: BlossomServerConfig[];
}

export function normalizeRuntimeServerUrl(url: string): string {
  return url.trim().replace(/\/+$/, '');
}

export function normalizeRuntimeRelayUrl(url: string): string {
  return url.trim().replace(/\/+$/, '');
}

function normalizeBlossomServer(server: BlossomServerConfig): BlossomServerConfig | null {
  const url = normalizeRuntimeServerUrl(server.url);
  if (!url) return null;
  return {
    url,
    read: server.read ?? true,
    write: server.write ?? false,
    ...(server.preferBatchReads === true ? { preferBatchReads: true } : {}),
  };
}

function uniqueRelayUrls(relays: readonly string[]): string[] {
  const seen = new Set<string>();
  const normalized: string[] = [];
  for (const relay of relays) {
    const url = normalizeRuntimeRelayUrl(relay);
    if (!url || seen.has(url)) continue;
    seen.add(url);
    normalized.push(url);
  }
  return normalized;
}

export function getRuntimeHtreeServerUrl(windowLike?: HtreeRuntimeWindowLike): string | null {
  const serverUrl = getInjectedHtreeServerUrl(windowLike);
  if (!serverUrl) return null;
  return normalizeRuntimeServerUrl(serverUrl);
}

export function getRuntimeNostrRelayUrl(windowLike?: HtreeRuntimeWindowLike): string | null {
  const serverUrl = getRuntimeHtreeServerUrl(windowLike);
  if (!serverUrl) return null;

  try {
    const url = new URL(serverUrl);
    if (url.protocol === 'http:') {
      url.protocol = 'ws:';
    } else if (url.protocol === 'https:') {
      url.protocol = 'wss:';
    } else {
      return null;
    }
    url.pathname = '/ws';
    url.search = '';
    url.hash = '';
    return normalizeRuntimeRelayUrl(url.toString());
  } catch {
    return null;
  }
}

export function getRuntimeBlossomServer(windowLike?: HtreeRuntimeWindowLike): BlossomServerConfig | null {
  const serverUrl = getRuntimeHtreeServerUrl(windowLike);
  if (!serverUrl) return null;
  return {
    url: serverUrl,
    read: true,
    write: true,
  };
}

export function getRuntimeNostrRelays(
  relays: readonly string[],
  options: RuntimeNetworkOptions = {},
): string[] {
  const runtimeRelayUrl = getRuntimeNostrRelayUrl(options.windowLike);
  if (runtimeRelayUrl) {
    return [runtimeRelayUrl];
  }
  return uniqueRelayUrls(relays);
}

export function getRuntimeBlossomServers(
  servers: readonly BlossomServerConfig[],
  options: RuntimeNetworkOptions = {},
): BlossomServerConfig[] {
  const merged = new Map<string, BlossomServerConfig>();
  const runtimeServer = getRuntimeBlossomServer(options.windowLike);
  const candidates = runtimeServer ? [runtimeServer, ...servers] : [...servers];

  for (const candidate of candidates) {
    const normalized = normalizeBlossomServer(candidate);
    if (!normalized) continue;

    const existing = merged.get(normalized.url);
    if (existing) {
      const preferBatchReads = existing.preferBatchReads || normalized.preferBatchReads;
      merged.set(normalized.url, {
        url: normalized.url,
        read: existing.read || normalized.read,
        write: existing.write || normalized.write,
        ...(preferBatchReads ? { preferBatchReads: true } : {}),
      });
      continue;
    }

    merged.set(normalized.url, normalized);
  }

  return Array.from(merged.values());
}

export function resolveRuntimeEndpoints(
  options: ResolveRuntimeEndpointsOptions = {},
): RuntimeEndpoints {
  return {
    htreeServerUrl: getRuntimeHtreeServerUrl(options.windowLike),
    nostrRelays: getRuntimeNostrRelays(options.relays ?? [], options),
    blossomServers: getRuntimeBlossomServers(options.blossomServers ?? [], options),
  };
}
