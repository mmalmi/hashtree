/**
 * Shared zap utilities for NIP-57 zap receipts
 */
import { SvelteSet } from 'svelte/reactivity';
import { ndk } from '../nostr';
import { KIND_ZAP_RECEIPT } from './constants';
import { NDKEvent, type NDKFilter } from 'ndk';

export interface Zap {
  id: string;
  senderPubkey: string;
  amountSats: number;
  comment?: string;
  createdAt: number;
}

/**
 * Decode amount from bolt11 invoice
 */
export function decodeBolt11Amount(bolt11: string): number {
  try {
    const lower = bolt11.toLowerCase();
    const match = lower.match(/^ln(?:bc|tb|tbs)(\d+)([munp])?/);
    if (!match) return 0;

    let amount = parseInt(match[1], 10);
    const multiplier = match[2];

    switch (multiplier) {
      case 'm': amount = amount * 100000; break;
      case 'u': amount = amount * 100; break;
      case 'n': amount = Math.ceil(amount / 10); break;
      case 'p': amount = Math.ceil(amount / 10000); break;
      default: amount = amount * 100000000;
    }

    return amount;
  } catch {
    return 0;
  }
}

/**
 * Parse a zap receipt event into a Zap object
 */
export function parseZapReceipt(event: NDKEvent): Zap | null {
  try {
    const descriptionTag = event.tags.find(t => t[0] === 'description');
    if (!descriptionTag || !descriptionTag[1]) return null;

    const zapRequest = JSON.parse(descriptionTag[1]);
    const senderPubkey = zapRequest.pubkey;
    if (!senderPubkey) return null;

    let amountSats = 0;
    const bolt11Tag = event.tags.find(t => t[0] === 'bolt11');
    if (bolt11Tag && bolt11Tag[1]) {
      amountSats = decodeBolt11Amount(bolt11Tag[1]);
    }
    if (!amountSats) {
      const amountTag = zapRequest.tags?.find((t: string[]) => t[0] === 'amount');
      if (amountTag && amountTag[1]) {
        amountSats = Math.floor(parseInt(amountTag[1], 10) / 1000);
      }
    }

    if (!amountSats) return null;

    return {
      id: event.id!,
      senderPubkey,
      amountSats,
      comment: zapRequest.content || undefined,
      createdAt: event.created_at || 0,
    };
  } catch {
    return null;
  }
}

/**
 * Create a zaps subscription that updates a reactive array
 * @param filter - NDK filter (e.g., { '#p': [pubkey] } or { '#i': [identifier] })
 * @param onZap - Callback when a new zap is received
 * @returns Cleanup function to stop subscription
 */
export function subscribeToZaps(
  filter: Partial<NDKFilter>,
  onZap: (zap: Zap) => void
): () => void {
  const seenIds = new SvelteSet<string>();

  const fullFilter: NDKFilter = {
    kinds: [KIND_ZAP_RECEIPT as number],
    ...filter,
  };

  const subscription = ndk.subscribe(fullFilter, { closeOnEose: false });

  subscription.on('event', (event: NDKEvent) => {
    if (!event.id) return;
    if (seenIds.has(event.id)) return;

    const zap = parseZapReceipt(event);
    if (!zap) return;

    seenIds.add(event.id);
    onZap(zap);
  });

  return () => subscription.stop();
}

/**
 * Helper to insert a zap into a sorted array (newest first)
 */
export function insertZapSorted(zaps: Zap[], zap: Zap): Zap[] {
  const index = zaps.findIndex(z => z.createdAt < zap.createdAt);
  if (index === -1) {
    return [...zaps, zap];
  }
  return [...zaps.slice(0, index), zap, ...zaps.slice(index)];
}
