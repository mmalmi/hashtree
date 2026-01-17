/**
 * Relay Management
 *
 * Relays run in the worker. This file provides relay status tracking
 * by polling the worker and updating the nostrStore.
 */
import { nostrStore, type RelayStatus, type RelayInfo } from './store';
import { settingsStore, DEFAULT_NETWORK_SETTINGS } from '../stores/settings';
import { getWorkerAdapter } from '../lib/workerInit';

// Normalize relay URL (remove trailing slash)
export function normalizeRelayUrl(url: string): string {
  return url.replace(/\/$/, '');
}

/**
 * Update relay status by polling the worker
 */
export async function updateConnectedRelayCount(): Promise<void> {
  const adapter = getWorkerAdapter();
  if (!adapter) {
    nostrStore.setConnectedRelays(0);
    return;
  }

  try {
    const stats = await adapter.getRelayStats();

    // Get configured relays from settings or use defaults
    const settings = settingsStore.getState();
    const configuredRelays = settings.network?.relays?.length > 0
      ? settings.network.relays
      : DEFAULT_NETWORK_SETTINGS.relays;

    // Normalize configured relays for comparison
    const configuredNormalized = new Set(configuredRelays.map(normalizeRelayUrl));

    // Initialize status maps
    const statuses = new Map<string, RelayStatus>();
    const discoveredRelays: RelayInfo[] = [];
    let connected = 0;

    // Initialize all configured relays as disconnected
    for (const url of configuredRelays) {
      statuses.set(normalizeRelayUrl(url), 'disconnected');
    }

    // Update with actual statuses from worker
    for (const relay of stats) {
      const status: RelayStatus = relay.connected ? 'connected' : 'disconnected';
      const normalizedUrl = normalizeRelayUrl(relay.url);

      if (configuredNormalized.has(normalizedUrl)) {
        statuses.set(normalizedUrl, status);
      } else {
        discoveredRelays.push({ url: normalizedUrl, status });
      }

      if (relay.connected) {
        connected++;
      }
    }

    discoveredRelays.sort((a, b) => a.url.localeCompare(b.url));

    nostrStore.setConnectedRelays(connected);
    nostrStore.setRelayStatuses(statuses);
    nostrStore.setDiscoveredRelays(discoveredRelays);
  } catch (err) {
    console.error('[Relays] Failed to get relay stats:', err);
  }
}

/**
 * Initialize relay tracking
 * Polls worker periodically for relay status updates.
 */
export function initRelayTracking(): void {
  // Poll immediately
  void updateConnectedRelayCount();

  // Poll frequently for first 5 seconds (every 200ms), then slow down
  let pollCount = 0;
  const fastPollInterval = setInterval(() => {
    pollCount++;
    void updateConnectedRelayCount();
    if (pollCount >= 25) {
      // 25 * 200ms = 5 seconds
      clearInterval(fastPollInterval);
    }
  }, 200);

  // Regular polling after initial burst
  setInterval(updateConnectedRelayCount, 2000);
}
