<script lang="ts">
  /**
   * FollowsPage - list of followed users
   */
  import { nip19 } from 'nostr-tools';
  import { nostrStore } from '../nostr';
  import { createFollowsStore, followPubkey, unfollowPubkey } from '../stores/follows';
  import { getFollowsMe, socialGraphStore } from '../utils/socialGraph';
  import { Avatar, Name, Badge } from './User';
  import { BackButton } from './ui';
  import InfiniteScroll from './InfiniteScroll.svelte';
  import { SvelteMap, SvelteSet } from 'svelte/reactivity';

  interface Props {
    npub?: string;
  }

  let { npub }: Props = $props();

  // Current user state
  let myPubkey = $derived($nostrStore.pubkey);
  let isLoggedIn = $derived($nostrStore.isLoggedIn);

  // Decode npub to hex pubkey
  let pubkeyHex = $derived.by(() => {
    if (!npub) return '';
    try {
      const decoded = nip19.decode(npub);
      return decoded.data as string;
    } catch {
      return '';
    }
  });

  // eslint-disable-next-line @typescript-eslint/no-unused-vars -- reserved for own-profile-specific UI
  let _isOwnProfile = $derived(myPubkey === pubkeyHex);

  // Follows store for the viewed profile
  let profileFollowsStore = $derived(pubkeyHex ? createFollowsStore(pubkeyHex) : null);
  let profileFollows = $state<string[]>([]);

  $effect(() => {
    if (!profileFollowsStore) {
      profileFollows = [];
      return;
    }
    const unsub = profileFollowsStore.subscribe(value => {
      profileFollows = value?.follows || [];
    });
    return () => {
      unsub();
      profileFollowsStore?.destroy();
    };
  });

  // My follows store (for follow/unfollow buttons)
  let myFollowsStore = $derived(myPubkey ? createFollowsStore(myPubkey) : null);
  let myFollows = $state<string[]>([]);

  $effect(() => {
    if (!myFollowsStore) {
      myFollows = [];
      return;
    }
    const unsub = myFollowsStore.subscribe(value => {
      myFollows = value?.follows || [];
    });
    return () => {
      unsub();
      myFollowsStore?.destroy();
    };
  });

  // Pre-compute "follows me" status for all displayed users (batch reactivity)
  let followsMeMap = $derived.by(() => {
    $socialGraphStore.version; // Single subscription for all
    const map = new SvelteMap<string, boolean>();
    for (const pubkey of profileFollows.slice(0, displayCount)) {
      map.set(pubkey, getFollowsMe(pubkey));
    }
    return map;
  });

  // Track loading state per pubkey for follow/unfollow
  const loadingPubkeys = new SvelteSet<string>();

  async function handleFollowToggle(targetPubkey: string) {
    if (!isLoggedIn || targetPubkey === myPubkey) return;

    loadingPubkeys.add(targetPubkey);

    const isCurrentlyFollowing = myFollows.includes(targetPubkey);
    if (isCurrentlyFollowing) {
      await unfollowPubkey(targetPubkey);
    } else {
      await followPubkey(targetPubkey);
    }

    loadingPubkeys.delete(targetPubkey);
  }

  function isFollowingUser(pubkey: string): boolean {
    return myFollows.includes(pubkey);
  }

  // Infinite scroll state
  const INITIAL_COUNT = 20;
  const LOAD_MORE_COUNT = 20;
  let displayCount = $state(INITIAL_COUNT);
  let loadingMore = $state(false);

  function loadMore() {
    if (loadingMore || displayCount >= profileFollows.length) return;
    loadingMore = true;
    // Small delay to prevent rapid-fire loading
    setTimeout(() => {
      displayCount = Math.min(displayCount + LOAD_MORE_COUNT, profileFollows.length);
      loadingMore = false;
    }, 100);
  }

  // Reset display count when profile changes
  $effect(() => {
    pubkeyHex;
    displayCount = INITIAL_COUNT;
  });
</script>

<div class="flex-1 flex flex-col min-h-0 bg-surface-0 overflow-y-auto" data-scrollable>
  <!-- Header -->
  <div class="shrink-0 px-4 py-3 border-b border-surface-3 bg-surface-1">
    <div class="flex items-center gap-3 max-w-2xl mx-auto">
      <BackButton href={npub ? `/${npub}/profile` : '/'} />
      {#if pubkeyHex}
        <a href={`#/${npub}/profile`} class="shrink-0">
          <Avatar pubkey={pubkeyHex} size={32} />
        </a>
        <div class="min-w-0 flex-1">
          <a href={`#/${npub}/profile`} class="font-medium text-text-1 hover:underline truncate block">
            <Name pubkey={pubkeyHex} />
          </a>
        </div>
        <span class="text-text-3 text-sm">Following ({profileFollows.length})</span>
      {/if}
    </div>
  </div>

  <!-- Content -->
  <div class="flex-1 min-h-0">
    <div class="max-w-2xl mx-auto">
      {#if profileFollows.length === 0}
        <div class="p-6 text-center text-muted">
          Not following anyone yet
        </div>
      {:else}
        <InfiniteScroll onLoadMore={loadMore} loading={loadingMore}>
          <div class="divide-y divide-surface-2">
            {#each profileFollows.slice(0, displayCount) as followedPubkey (followedPubkey)}
              {@const isLoading = loadingPubkeys.has(followedPubkey)}
              {@const amFollowing = isFollowingUser(followedPubkey)}
              {@const isSelf = followedPubkey === myPubkey}
              {@const theyFollowMe = followsMeMap.get(followedPubkey) ?? false}
              <div class="flex items-center gap-3 p-4 hover:bg-surface-1 transition-colors">
                <!-- Avatar -->
                <a
                  href="#/{nip19.npubEncode(followedPubkey)}"
                  class="shrink-0"
                >
                  <Avatar pubkey={followedPubkey} size={44} showBadge={true} />
                </a>

                <!-- Name and info -->
                <div class="flex-1 min-w-0">
                  <a
                    href="#/{nip19.npubEncode(followedPubkey)}"
                    class="font-medium text-text-1 hover:underline truncate block"
                  >
                    <Name pubkey={followedPubkey} />
                  </a>
                  {#if theyFollowMe}
                    <div class="text-xs text-accent">Follows you</div>
                  {/if}
                </div>

                <!-- Follow/Unfollow button -->
                {#if isLoggedIn && !isSelf}
                  <button
                    onclick={() => handleFollowToggle(followedPubkey)}
                    disabled={isLoading}
                    class="shrink-0 {amFollowing ? 'btn-ghost' : 'btn-success'} text-sm"
                  >
                    {isLoading ? '...' : amFollowing ? 'Unfollow' : 'Follow'}
                  </button>
                {:else if isSelf}
                  <span class="text-xs text-accent flex items-center gap-1 shrink-0">
                    <Badge pubKeyHex={followedPubkey} size="sm" /> You
                  </span>
                {/if}
              </div>
            {/each}
          </div>
        </InfiniteScroll>
      {/if}
    </div>
  </div>
</div>
