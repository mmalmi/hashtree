import {
  createNostrRuntime,
  type NostrRuntime,
  type NostrFilter as EventFilter,
  type RuntimeCacheMode,
  type RuntimeCompletion,
  type RuntimeEventInfo,
  type RuntimePublishResult,
  type RuntimeQueryOptions,
  type RuntimeQueryResult,
  type RuntimeSource,
  type RuntimeSubscription,
} from 'nostr-pubsub';
import { fromHex, type Store } from '@hashtree/core';
import { HASHTREE_LABEL, HASHTREE_ROOT_KINDS } from '@hashtree/nostr';
import type { SignedEvent, NostrFilter } from './protocol.js';
import { createWorkerEventStore } from './eventIndex.js';
import { signEvent } from './signing.js';

let runtime: NostrRuntime | null = null;
let onEvent: ((subId: string, event: SignedEvent, info: RuntimeEventInfo) => void | Promise<void>) | null = null;
let onEose: ((subId: string, status: RuntimeCompletion) => void) | null = null;
const subscriptions = new Map<string, RuntimeSubscription>();

export async function initNostrRuntime(relays: string[], options: { store: Store; storeName: string }): Promise<void> {
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

function current(): NostrRuntime {
  if (!runtime) throw new Error('Nostr event runtime is not initialized');
  return runtime;
}

export function setOnEvent(callback: typeof onEvent): void { onEvent = callback; }
export function setOnEose(callback: typeof onEose): void { onEose = callback; }

export function subscribe(subId: string, filters: NostrFilter[], options: { cache?: RuntimeCacheMode } = {}): void {
  unsubscribe(subId);
  subscriptions.set(subId, current().subscribe(filters as EventFilter[], {
    onEvent: (event, info) => { void Promise.resolve(onEvent?.(subId, event, info)).catch((error) => console.warn('[Worker event]', error)); },
    onEose: (status) => onEose?.(subId, status),
  }, options));
}

export function unsubscribe(subId: string): void {
  subscriptions.get(subId)?.close();
  subscriptions.delete(subId);
}

export function publish(event: SignedEvent): Promise<RuntimePublishResult> { return current().publish(event); }
export function query(filters: NostrFilter[], options: RuntimeQueryOptions = {}): Promise<RuntimeQueryResult> {
  return current().query(filters as EventFilter[], options);
}
export function addSource(source: RuntimeSource): void { current().addSource(source); }
export function removeSource(id: string): void { current().removeSource(id); }
export function setRelays(relays: string[]): void { current().setRelays(relays); }
export function getRelayStats() {
  return runtime?.getRelayStats().map((entry) => ({ ...entry, eventsReceived: 0, eventsSent: 0 })) ?? [];
}
export async function closeNostrRuntime(): Promise<void> {
  for (const subscription of subscriptions.values()) subscription.close();
  subscriptions.clear();
  const previous = runtime;
  runtime = null;
  await previous?.close();
}

type Signer = (event: { kind: number; created_at: number; content: string; tags: string[][] }) => Promise<SignedEvent>;
type BlossomPush = (hash: Uint8Array, key?: Uint8Array, treeName?: string) => Promise<{ pushed: number; skipped: number; failed: number }>;

export async function republishTrees(pubkey: string, _sign: Signer, push?: BlossomPush, prefix?: string): Promise<number> {
  const result = await query([{ kinds: [...HASHTREE_ROOT_KINDS], authors: [pubkey], '#l': [HASHTREE_LABEL] }]);
  const decodedPrefix = prefix ? decodeURIComponent(prefix) : undefined;
  let count = 0;
  for (const event of result.events) {
    const name = event.tags.find((tag) => tag[0] === 'd')?.[1];
    if (decodedPrefix && !name?.startsWith(decodedPrefix)) continue;
    // Deleted roots must also propagate, preventing old remote roots resurfacing.
    const receipt = await publish(event);
    if (receipt.accepted || receipt.queued) count += 1;
    const hash = event.tags.find((tag) => tag[0] === 'hash')?.[1];
    const key = event.tags.find((tag) => tag[0] === 'key')?.[1];
    if (hash && push) await push(fromHex(hash), key ? fromHex(key) : undefined, name);
  }
  return count;
}

export async function republishTree(pubkey: string, treeName: string): Promise<boolean> {
  const result = await query([{ kinds: [...HASHTREE_ROOT_KINDS], authors: [pubkey], '#d': [treeName], '#l': [HASHTREE_LABEL] }]);
  const event = result.events.sort((a, b) => b.created_at - a.created_at || a.id.localeCompare(b.id))[0];
  if (!event) return false;
  const receipt = await publish(event);
  return receipt.accepted || receipt.queued;
}
