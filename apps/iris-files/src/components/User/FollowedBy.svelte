<script lang="ts">
  /**
   * FollowedBy component - shows which friends follow a user
   */
  import { nip19 } from 'nostr-tools';
  import { Avatar } from './index';
  import { Name } from './index';
  import { followDistance, followedByFriends, socialGraphStore } from '../../utils/socialGraph';
  import { nostrStore } from '../../nostr';

  const MAX_AVATARS = 3;
  const MAX_NAMES = 3;

  interface Props {
    pubkey: string;
    class?: string;
  }

  let { pubkey, class: className = '' }: Props = $props();

  let myPubkey = $derived($nostrStore.pubkey);

  // Re-derive when graph version changes
  let distance = $derived.by(() => {
    $socialGraphStore.version;
    return followDistance(pubkey);
  });
  let friends = $derived.by(() => {
    $socialGraphStore.version;
    return followedByFriends(pubkey);
  });

  let friendsArray = $derived(Array.from(friends).slice(0, MAX_AVATARS));
  let total = $derived(friends.size);

  // Don't show for self
  let isSelf = $derived(pubkey === myPubkey);

  function getNpub(pk: string): string {
    try {
      return nip19.npubEncode(pk);
    } catch {
      return pk;
    }
  }
</script>

{#if !isSelf}
  {#if total === 0}
    {#if distance === 2}
      <div class="text-sm text-text-2 {className}">
        Not followed by anyone you follow
      </div>
    {:else if distance === 3}
      <div class="text-sm text-text-2 {className}">
        Followed by friends of friends
      </div>
    {/if}
  {:else}
    <div class="flex items-center gap-2 {className}">
      <!-- Avatar stack -->
      <div class="flex -space-x-2">
        {#each friendsArray as pk (pk)}
          <a
            href="#/{getNpub(pk)}"
            class="rounded-full ring-2 ring-surface-0 hover:z-10"
          >
            <Avatar pubkey={pk} size={24} />
          </a>
        {/each}
      </div>

      <!-- Names -->
      <div class="text-sm text-text-2 min-w-0 truncate">
        <span>Followed by </span>
        {#each friendsArray.slice(0, MAX_NAMES) as pk, i (pk)}
          {#if i > 0}
            {#if i === friendsArray.length - 1 || i === MAX_NAMES - 1}
              <span> and </span>
            {:else}
              <span>, </span>
            {/if}
          {/if}
          <a
            href="#/{getNpub(pk)}"
            class="text-text-1 hover:underline"
          >
            <Name pubkey={pk} />
          </a>
        {/each}
        {#if total > MAX_NAMES}
          <span> and {total - MAX_NAMES} other{total - MAX_NAMES !== 1 ? 's' : ''}</span>
        {/if}
      </div>
    </div>
  {/if}
{/if}
