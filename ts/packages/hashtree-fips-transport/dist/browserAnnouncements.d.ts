import { RetainedPeerDiscovery, type NostrEventReader, type RuntimeSource } from 'nostr-pubsub';
/** Only the public discovery namespace is replayable; app readers keep their own admission. */
export declare function openBrowserAnnouncements(owner: string, scope: string, relays: readonly string[], reader: NostrEventReader | undefined, onError: (error: Error) => void): Promise<{
    discovery: RetainedPeerDiscovery;
    retained: NostrEventReader;
    attach: (source: RuntimeSource) => void;
    close: () => Promise<void>;
}>;
//# sourceMappingURL=browserAnnouncements.d.ts.map