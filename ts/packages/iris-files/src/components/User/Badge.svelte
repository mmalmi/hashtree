<script lang="ts">
  /**
   * Social graph badge component
   * Shows checkmark based on follow distance
   */
  import { followDistance, followedByFriends, socialGraphStore } from '../../utils/socialGraph';
  import { nostrStore } from '../../nostr';

  interface Props {
    pubKeyHex: string;
    size?: 'sm' | 'md' | 'lg';
    class?: string;
  }

  let { pubKeyHex, size = 'md', class: className = '' }: Props = $props();

  const sizeConfig = {
    sm: { badge: 'w-3 h-3', icon: 8 },
    md: { badge: 'w-4 h-4', icon: 10 },
    lg: { badge: 'w-5 h-5', icon: 12 },
  };

  let publicKey = $derived($nostrStore.pubkey);
  let loggedIn = $derived(!!publicKey);

  // Re-derive when graph version changes
  let distance = $derived.by(() => {
    $socialGraphStore.version;
    return followDistance(pubKeyHex);
  });
  let friends = $derived.by(() => {
    $socialGraphStore.version;
    return followedByFriends(pubKeyHex);
  });

  let shouldShow = $derived(loggedIn && pubKeyHex && distance <= 2);

  let tooltip = $derived.by(() => {
    if (distance === 0) return 'You';
    if (distance === 1) return 'Following';
    const friendCount = friends.size;
    return `Followed by ${friendCount} friend${friendCount !== 1 ? 's' : ''}`;
  });

  let badgeClass = $derived.by(() => {
    if (distance === 0 || distance === 1) return 'bg-blue-500';
    const friendCount = friends.size;
    return friendCount > 10 ? 'bg-purple-500' : 'bg-gray-500';
  });

  let config = $derived(sizeConfig[size]);
</script>

{#if shouldShow}
  <span
    class="rounded-full flex items-center justify-center {config.badge} text-white {badgeClass} {className}"
    title={tooltip}
  >
    <svg
      width={config.icon}
      height={config.icon}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="3"
      stroke-linecap="round"
      stroke-linejoin="round"
    >
      <polyline points="20 6 9 17 4 12" />
    </svg>
  </span>
{/if}
