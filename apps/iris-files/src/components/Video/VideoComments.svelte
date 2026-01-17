<script lang="ts">
  /**
   * VideoComments - NIP-22 comments and NIP-57 zaps for videos
   * Subscribes to comments and zap receipts from Nostr relays
   * Zaps and comments are shown in a unified timeline
   */
  import { untrack } from 'svelte';
  import { SvelteSet } from 'svelte/reactivity';
  import { nip19 } from 'nostr-tools';
  import { ndk, nostrStore } from '../../nostr';
  import { Avatar, Name } from '../User';
  import { NDKEvent, type NDKFilter, type NDKSubscription } from 'ndk';
  import { getFollowDistance } from '../../utils/socialGraph';
  import { subscribeToZaps, insertZapSorted, type Zap } from '../../utils/zaps';

  interface Props {
    npub?: string;  // Optional - may not be available for nhash paths
    treeName?: string;
    nhash?: string;  // For content-addressed permalinks
    filename?: string;  // Optional filename for nhash/filename tagging
  }

  let { npub, treeName, nhash, filename }: Props = $props();

  // Derive owner pubkey from npub if available
  let ownerPubkey = $derived.by(() => {
    if (!npub) return null;
    try {
      const decoded = nip19.decode(npub);
      if (decoded.type === 'npub') return decoded.data as string;
    } catch {}
    return null;
  });

  interface Comment {
    id: string;
    content: string;
    authorPubkey: string;
    createdAt: number;
    replyTo?: string;
  }

  // Unified timeline item
  type TimelineItem =
    | { type: 'comment'; data: Comment }
    | { type: 'zap'; data: Zap };

  let allComments = $state<Comment[]>([]);
  let allZaps = $state<Zap[]>([]);
  let newComment = $state('');
  let submitting = $state(false);
  let showUnknown = $state(true); // Show all by default
  let subscription = $state<NDKSubscription | null>(null);
  let zapCleanup = $state<(() => void) | null>(null);
  const seenIds = new SvelteSet<string>();

  let isLoggedIn = $derived($nostrStore.isLoggedIn);
  let userPubkey = $derived($nostrStore.pubkey);

  // Total zaps summary
  let totalZapSats = $derived(allZaps.reduce((sum, z) => sum + z.amountSats, 0));

  // Merge comments and zaps into unified timeline, sorted by time (newest first)
  let timeline = $derived.by(() => {
    const items: TimelineItem[] = [
      ...allComments.map(c => ({ type: 'comment' as const, data: c })),
      ...allZaps.map(z => ({ type: 'zap' as const, data: z })),
    ];
    items.sort((a, b) => {
      const timeA = a.type === 'comment' ? a.data.createdAt : a.data.createdAt;
      const timeB = b.type === 'comment' ? b.data.createdAt : b.data.createdAt;
      return timeB - timeA; // Newest first
    });
    return items;
  });

  // Filter timeline by social graph
  let filteredTimeline = $derived.by(() => {
    if (showUnknown) return timeline;
    return timeline.filter(item => {
      const pubkey = item.type === 'comment' ? item.data.authorPubkey : item.data.senderPubkey;
      const distance = getFollowDistance(pubkey);
      return distance < 1000;
    });
  });

  let unknownCount = $derived(timeline.length - timeline.filter(item => {
    const pubkey = item.type === 'comment' ? item.data.authorPubkey : item.data.senderPubkey;
    return getFollowDistance(pubkey) < 1000;
  }).length);

  // Comment identifiers - we may have both npub/treeName and nhash for cross-linking
  let npubId = $derived(npub && treeName ? `${npub}/${treeName}` : null);
  let nhashId = $derived(nhash || null);
  let nhashFileId = $derived(!npub && nhash && filename ? `${nhash}/${filename}` : null);

  // Primary identifier for subscribing
  let primaryId = $derived(npubId || nhashId);

  // Subscribe to comments and zaps when primaryId changes
  $effect(() => {
    const id = primaryId;
    if (!id) return;

    untrack(() => {
      subscribeToComments(id);
      // Subscribe to zaps using shared utility
      allZaps = [];
      zapCleanup = subscribeToZaps({ '#i': [id] }, (zap) => {
        allZaps = insertZapSorted(allZaps, zap);
      });
    });

    return () => {
      if (subscription) {
        subscription.stop();
      }
      if (zapCleanup) {
        zapCleanup();
      }
    };
  });

  function subscribeToComments(id: string) {
    allComments = [];
    seenIds.clear();

    const filter: NDKFilter = {
      kinds: [1111 as number],
      '#i': [id],
    };

    subscription = ndk.subscribe(filter, { closeOnEose: false });

    subscription.on('event', (event: NDKEvent) => {
      if (!event.id || !event.pubkey) return;
      if (seenIds.has(event.id)) return;
      seenIds.add(event.id);

      const comment: Comment = {
        id: event.id,
        content: event.content || '',
        authorPubkey: event.pubkey,
        createdAt: event.created_at || 0,
      };

      const index = allComments.findIndex(c => c.createdAt < comment.createdAt);
      if (index === -1) {
        allComments = [...allComments, comment];
      } else {
        allComments = [...allComments.slice(0, index), comment, ...allComments.slice(index)];
      }
    });
  }

  async function submitComment() {
    if (!newComment.trim() || !isLoggedIn || submitting || !primaryId) return;

    submitting = true;
    try {
      const event = new NDKEvent(ndk);
      event.kind = 1111;
      event.content = newComment.trim();

      const tags: string[][] = [['k', 'video']];
      if (nhashId) tags.push(['i', nhashId]);
      if (nhashFileId) tags.push(['i', nhashFileId]);
      if (npubId) tags.push(['i', npubId]);
      if (ownerPubkey) tags.push(['p', ownerPubkey]);

      event.tags = tags;
      await event.sign();

      if (event.id) {
        seenIds.add(event.id);
        allComments = [{
          id: event.id,
          content: event.content,
          authorPubkey: userPubkey || '',
          createdAt: event.created_at || Math.floor(Date.now() / 1000),
        }, ...allComments];
      }

      await event.publish();
      newComment = '';
    } catch (e) {
      console.error('Failed to post comment:', e);
      alert('Failed to post comment');
    } finally {
      submitting = false;
    }
  }

  function formatTime(timestamp: number): string {
    const date = new Date(timestamp * 1000);
    const now = new Date();
    const diff = now.getTime() - date.getTime();

    const minutes = Math.floor(diff / 60000);
    if (minutes < 1) return 'just now';
    if (minutes < 60) return `${minutes}m ago`;

    const hours = Math.floor(minutes / 60);
    if (hours < 24) return `${hours}h ago`;

    const days = Math.floor(hours / 24);
    if (days < 7) return `${days}d ago`;

    return date.toLocaleDateString();
  }
</script>

<div class="border-t border-surface-3 pt-6 pb-12">
  <!-- Zaps summary -->
  {#if allZaps.length > 0}
    <div class="flex items-center gap-3 mb-4 p-3 bg-surface-1 rounded-lg" data-testid="zaps-summary">
      <span class="i-lucide-dollar-sign text-yellow-400 text-xl"></span>
      <div>
        <span class="font-semibold text-yellow-400" data-testid="zaps-total">
          $ {totalZapSats.toLocaleString()} sats
        </span>
        <span class="text-text-3 text-sm ml-2">
          from {allZaps.length} tip{allZaps.length !== 1 ? 's' : ''}
        </span>
      </div>
    </div>
  {/if}

  <div class="flex items-center justify-between mb-4">
    <h2 class="text-xl font-semibold text-text-1">
      Comments {#if timeline.length > 0}<span class="text-text-3 font-normal">({timeline.length})</span>{/if}
    </h2>

    {#if timeline.length > 0}
      <label class="flex items-center gap-2 text-xs text-text-2 cursor-pointer">
        <input
          type="checkbox"
          bind:checked={showUnknown}
          class="w-4 h-4 accent-accent"
        />
        Show from unknown users{#if unknownCount > 0} ({unknownCount}){/if}
      </label>
    {/if}
  </div>

  <!-- Add comment -->
  {#if isLoggedIn}
    <div class="flex gap-3 mb-6">
      <div class="shrink-0">
        <Avatar pubkey={userPubkey || ''} size={40} />
      </div>
      <div class="flex-1">
        <textarea
          bind:value={newComment}
          placeholder="Add a comment..."
          class="textarea w-full resize-none text-sm"
          rows="2"
        ></textarea>
        <div class="flex justify-end mt-2">
          <button
            onclick={submitComment}
            disabled={!newComment.trim() || submitting}
            class="btn-primary px-4 py-2 disabled:opacity-50"
          >
            {submitting ? 'Posting...' : 'Comment'}
          </button>
        </div>
      </div>
    </div>
  {:else}
    <div class="bg-surface-1 rounded-lg p-4 text-center text-text-3 mb-6">
      Sign in to leave a comment
    </div>
  {/if}

  <!-- Unified timeline -->
  {#if filteredTimeline.length === 0}
    <div class="text-center py-8 text-text-3">
      {#if !showUnknown && unknownCount > 0}
        No activity from people you follow.
        <button onclick={() => showUnknown = true} class="text-accent hover:underline ml-1">
          Show all {unknownCount}
        </button>
      {:else}
        No comments yet. Be the first!
      {/if}
    </div>
  {:else}
    <div class="space-y-4">
      {#each filteredTimeline as item (item.data.id)}
        {#if item.type === 'comment'}
          <div class="flex gap-3">
            <a href={`#/${nip19.npubEncode(item.data.authorPubkey)}`} class="shrink-0">
              <Avatar pubkey={item.data.authorPubkey} size={40} />
            </a>
            <div class="flex-1 min-w-0">
              <div class="flex items-center gap-2 mb-1">
                <a href={`#/${nip19.npubEncode(item.data.authorPubkey)}`} class="font-medium text-text-1 hover:text-accent no-underline">
                  <Name pubkey={item.data.authorPubkey} />
                </a>
                <span class="text-xs text-text-3">{formatTime(item.data.createdAt)}</span>
              </div>
              <p class="text-text-2 whitespace-pre-wrap break-words">{item.data.content}</p>
            </div>
          </div>
        {:else}
          <!-- Zap item -->
          <div class="flex gap-3 p-3 bg-surface-1 rounded-lg" data-testid="zap-item">
            <a href={`#/${nip19.npubEncode(item.data.senderPubkey)}`} class="shrink-0">
              <Avatar pubkey={item.data.senderPubkey} size={40} />
            </a>
            <div class="flex-1 min-w-0">
              <div class="flex items-center gap-2 flex-wrap">
                <a href={`#/${nip19.npubEncode(item.data.senderPubkey)}`} class="font-medium text-text-1 hover:text-accent no-underline">
                  <Name pubkey={item.data.senderPubkey} />
                </a>
                <span class="text-yellow-400 font-semibold">
                  $ {item.data.amountSats.toLocaleString()} sats
                </span>
                <span class="text-xs text-text-3">{formatTime(item.data.createdAt)}</span>
              </div>
              {#if item.data.comment}
                <p class="text-text-2 text-sm mt-1 whitespace-pre-wrap break-words">{item.data.comment}</p>
              {/if}
            </div>
          </div>
        {/if}
      {/each}
    </div>
  {/if}
</div>
