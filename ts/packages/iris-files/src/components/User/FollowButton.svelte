<script lang="ts">
  /**
   * FollowButton - Self-contained follow/unfollow button
   * Handles its own state and publishes kind 3 events
   */
  import { nip19 } from 'nostr-tools';
  import { nostrStore } from '../../nostr';
  import { followPubkey, unfollowPubkey, createFollowsStore } from '../../stores/follows';

  interface Props {
    pubkey: string;
  }

  let { pubkey }: Props = $props();

  let isHovering = $state(false);
  let loading = $state(false);

  // Current user
  let currentUserPubkey = $derived($nostrStore.pubkey);
  let isLoggedIn = $derived($nostrStore.isLoggedIn);

  // Convert npub to hex if needed
  let pubkeyHex = $derived.by(() => {
    if (!pubkey) return '';
    if (pubkey.startsWith('npub1')) {
      try {
        const decoded = nip19.decode(pubkey);
        return decoded.data as string;
      } catch {
        return '';
      }
    }
    return pubkey;
  });

  // Don't show for own profile or if not logged in
  let shouldShow = $derived(isLoggedIn && pubkeyHex && currentUserPubkey && pubkeyHex !== currentUserPubkey);

  // Subscribe to current user's follows for reactive updates
  let followsStore = $derived(currentUserPubkey ? createFollowsStore(currentUserPubkey) : null);
  let myFollows = $state<string[]>([]);

  $effect(() => {
    if (!followsStore) {
      myFollows = [];
      return;
    }
    const unsub = followsStore.subscribe(value => {
      myFollows = value?.follows || [];
    });
    return () => {
      unsub();
      followsStore?.destroy();
    };
  });

  // Check if following (reactive)
  let isFollowing = $derived(pubkeyHex ? myFollows.includes(pubkeyHex) : false);

  // Button text and style
  let buttonText = $derived.by(() => {
    if (isFollowing) {
      return isHovering ? 'Unfollow' : 'Following';
    }
    return 'Follow';
  });

  let buttonClass = $derived.by(() => {
    if (isFollowing) {
      return isHovering ? 'btn-danger' : 'btn-ghost';
    }
    return 'btn-primary';
  });

  async function handleClick() {
    if (!pubkeyHex || loading) return;

    loading = true;
    try {
      if (isFollowing) {
        await unfollowPubkey(pubkeyHex);
      } else {
        await followPubkey(pubkeyHex);
      }
    } catch (e) {
      console.error('Follow action failed:', e);
    } finally {
      loading = false;
    }
  }
</script>

{#if shouldShow}
  <button
    onclick={handleClick}
    onmouseenter={() => isHovering = true}
    onmouseleave={() => isHovering = false}
    class="{buttonClass}"
    disabled={loading}
  >
    {#if loading}
      <span class="i-lucide-loader-2 animate-spin"></span>
    {:else}
      {buttonText}
    {/if}
  </button>
{/if}
