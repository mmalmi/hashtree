import {
  IndexedDbEventBus, MemoryEventStore, NostrRuntime, RetainedPeerDiscovery, selectStoredEvents,
  type NostrEventReader, type RuntimeSource,
} from 'nostr-pubsub';
import { DEFAULT_FIPS_DISCOVERY_APP } from './constants.js';

/** Only the public discovery namespace is replayable; app readers keep their own admission. */
export async function openBrowserAnnouncements(owner: string, scope: string, relays: readonly string[],
  reader: NostrEventReader | undefined, onError: (error: Error) => void) {
  scope = scope.trim() || DEFAULT_FIPS_DISCOVERY_APP;
  const store = await IndexedDbEventBus.open('hashtree:public-peers:' + owner + ':' + scope,
    { maxEvents: 256, filters: [{ kinds: [37195], '#d': [scope] }] });
  const runtime = new NostrRuntime({ relays: [...relays], store: new MemoryEventStore(256, 8), onError });
  const discovery = new RetainedPeerDiscovery(runtime, store, onError);
  const retained: NostrEventReader = {
    query: async (filters, options) => {
      const reports = await Promise.all([
        store.query(filters, options),
        ...(reader ? [reader.query(filters, options)] : []),
      ]);
      const byId = new Map(reports.flatMap(report => report.events).map(item => [item.event.id, item]));
      return { complete: reports.every(report => report.complete !== false),
        events: selectStoredEvents([...byId.values()].map(item => item.event), filters, options)
          .map(event => byId.get(event.id)!),
      };
    },
  };
  return {
    discovery, retained,
    attach: (source: RuntimeSource) => runtime.addSource(source),
    close: async () => {
      try { await discovery.close(); } finally {
        try { await runtime.close(); } finally { store.close(); }
      }
    },
  };
}
