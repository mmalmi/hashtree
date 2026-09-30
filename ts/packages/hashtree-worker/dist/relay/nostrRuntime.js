import { createNostrRuntime, } from 'nostr-pubsub';
import { fromHex } from '@hashtree/core';
import { HASHTREE_LABEL, HASHTREE_ROOT_KINDS } from '@hashtree/nostr';
import { createWorkerEventStore } from './eventIndex.js';
import { signEvent } from './signing.js';
let runtime = null;
let onEvent = null;
let onEose = null;
const subscriptions = new Map();
export async function initNostrRuntime(relays, options) {
    await closeNostrRuntime();
    runtime = createNostrRuntime({
        relays,
        store: createWorkerEventStore(options.store, options.storeName),
        signAuthEvent: (_relay, template) => signEvent(template),
        maxSubscriptions: 256,
        maxFilterBytesPerBatch: 64 * 1024,
        onError: (error) => console.warn('[Worker pubsub]', error),
    });
}
function current() {
    if (!runtime)
        throw new Error('Nostr event runtime is not initialized');
    return runtime;
}
export function setOnEvent(callback) { onEvent = callback; }
export function setOnEose(callback) { onEose = callback; }
export function subscribe(subId, filters, options = {}) {
    unsubscribe(subId);
    subscriptions.set(subId, current().subscribe(filters, {
        onEvent: (event, info) => { void Promise.resolve(onEvent?.(subId, event, info)).catch((error) => console.warn('[Worker event]', error)); },
        onEose: (status) => onEose?.(subId, status),
    }, options));
}
export function unsubscribe(subId) {
    subscriptions.get(subId)?.close();
    subscriptions.delete(subId);
}
export function publish(event) { return current().publish(event); }
export function query(filters, options = {}) {
    return current().query(filters, options);
}
export function addSource(source) { current().addSource(source); }
export function removeSource(id) { current().removeSource(id); }
export function setRelays(relays) { current().setRelays(relays); }
export function getRelayStats() {
    return runtime?.getRelayStats().map((entry) => ({ ...entry, eventsReceived: 0, eventsSent: 0 })) ?? [];
}
export async function closeNostrRuntime() {
    for (const subscription of subscriptions.values())
        subscription.close();
    subscriptions.clear();
    const previous = runtime;
    runtime = null;
    await previous?.close();
}
export async function republishTrees(pubkey, _sign, push, prefix) {
    const result = await query([{ kinds: [...HASHTREE_ROOT_KINDS], authors: [pubkey], '#l': [HASHTREE_LABEL] }]);
    const decodedPrefix = prefix ? decodeURIComponent(prefix) : undefined;
    let count = 0;
    for (const event of result.events) {
        const name = event.tags.find((tag) => tag[0] === 'd')?.[1];
        if (decodedPrefix && !name?.startsWith(decodedPrefix))
            continue;
        // Deleted roots must also propagate, preventing old remote roots resurfacing.
        const receipt = await publish(event);
        if (receipt.accepted || receipt.queued)
            count += 1;
        const hash = event.tags.find((tag) => tag[0] === 'hash')?.[1];
        const key = event.tags.find((tag) => tag[0] === 'key')?.[1];
        if (hash && push)
            await push(fromHex(hash), key ? fromHex(key) : undefined, name);
    }
    return count;
}
export async function republishTree(pubkey, treeName) {
    const result = await query([{ kinds: [...HASHTREE_ROOT_KINDS], authors: [pubkey], '#d': [treeName], '#l': [HASHTREE_LABEL] }]);
    const event = result.events.sort((a, b) => b.created_at - a.created_at || a.id.localeCompare(b.id))[0];
    if (!event)
        return false;
    const receipt = await publish(event);
    return receipt.accepted || receipt.queued;
}
//# sourceMappingURL=nostrRuntime.js.map