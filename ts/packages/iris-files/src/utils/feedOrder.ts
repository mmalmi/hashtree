/**
 * Feed ordering utility
 * Shared logic for ordering video feeds with owner interleaving
 */

interface FeedItem {
  ownerPubkey: string | null;
  timestamp?: number;
}

/**
 * Order feed items with owner interleaving
 *
 * This prevents any single owner from dominating the feed by round-robin
 * mixing videos from different owners, ordered by most recent first.
 *
 * @param items - Array of feed items with ownerPubkey and timestamp
 * @param limit - Maximum number of items to return (optional)
 * @returns Ordered array with interleaved owners
 */
export function orderFeedWithInterleaving<T extends FeedItem>(items: T[], limit?: number): T[] {
  if (items.length === 0) return [];

  // Sort by timestamp first
  const sorted = [...items].sort((a, b) => (b.timestamp || 0) - (a.timestamp || 0));

  // Group by owner (each owner's videos already sorted by timestamp)
  const byOwner = new Map<string, T[]>();
  for (const item of sorted) {
    const owner = item.ownerPubkey || '';
    if (!byOwner.has(owner)) byOwner.set(owner, []);
    byOwner.get(owner)!.push(item);
  }

  // Sort owners by their most recent video (first entry in each list)
  const ownerList = Array.from(byOwner.keys()).sort((a, b) => {
    const aTime = byOwner.get(a)![0].timestamp || 0;
    const bTime = byOwner.get(b)![0].timestamp || 0;
    return bTime - aTime;
  });

  // Round-robin: latest from each owner, then next latest from each, etc.
  const mixed: T[] = [];
  const indices = new Map(ownerList.map(o => [o, 0]));
  const targetCount = limit ?? sorted.length;

  while (mixed.length < targetCount && mixed.length < sorted.length) {
    let addedAny = false;
    for (const owner of ownerList) {
      const videos = byOwner.get(owner)!;
      const idx = indices.get(owner)!;
      if (idx < videos.length) {
        mixed.push(videos[idx]);
        indices.set(owner, idx + 1);
        addedAny = true;
        if (mixed.length >= targetCount) break;
      }
    }
    if (!addedAny) break;
  }

  return mixed;
}
