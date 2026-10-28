import { IndexedDbEventBus, MemoryEventStore, NostrRuntime, RetainedPeerDiscovery, selectStoredEvents, } from 'nostr-pubsub';
import { DEFAULT_FIPS_DISCOVERY_APP } from './constants.js';
/** Only the public discovery namespace is replayable; app readers keep their own admission. */
export async function openBrowserAnnouncements(owner, scope, relays, reader, onError) {
    scope = scope.trim() || DEFAULT_FIPS_DISCOVERY_APP;
    const store = await IndexedDbEventBus.open('hashtree:public-peers:' + owner + ':' + scope, { maxEvents: 256, filters: [{ kinds: [37195], '#d': [scope] }] });
    const runtime = new NostrRuntime({ relays: [...relays], store: new MemoryEventStore(256, 8), onError });
    const discovery = new RetainedPeerDiscovery(runtime, store, onError);
    const retained = {
        query: async (filters, options) => {
            const reports = await Promise.all([
                store.query(filters, options),
                ...(reader ? [reader.query(filters, options)] : []),
            ]);
            const byId = new Map(reports.flatMap(report => report.events).map(item => [item.event.id, item]));
            return { complete: reports.every(report => report.complete !== false),
                events: selectStoredEvents([...byId.values()].map(item => item.event), filters, options)
                    .map(event => byId.get(event.id)),
            };
        },
    };
    return {
        discovery, retained,
        attach: (source) => runtime.addSource(source),
        close: async () => {
            try {
                await discovery.close();
            }
            finally {
                try {
                    await runtime.close();
                }
                finally {
                    store.close();
                }
            }
        },
    };
}
//# sourceMappingURL=browserAnnouncements.js.map