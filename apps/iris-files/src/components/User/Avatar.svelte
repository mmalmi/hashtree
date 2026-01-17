<script lang="ts">
  import { createProfileStore, getProfileName } from '../../stores/profile';
  import { animalName } from '../../utils/animalName';
  import Minidenticon from './Minidenticon.svelte';
  import Badge from './Badge.svelte';
  import ProxyImg from '../ProxyImg.svelte';

  interface Props {
    pubkey: string;
    size?: number;
    class?: string;
    showBadge?: boolean;
  }

  let { pubkey, size = 40, class: className = '', showBadge = false }: Props = $props();

  let profileStore = $derived(pubkey ? createProfileStore(pubkey) : null);
  let profile = $derived(profileStore ? $profileStore : null);
  let imgError = $state(false);

  // Reset error state when pubkey changes
  $effect(() => {
    pubkey; // depend on pubkey
    imgError = false;
  });

  let name = $derived(getProfileName(profile, pubkey) || (pubkey ? animalName(pubkey) : ''));

  // Auto-select badge size based on avatar size
  function getBadgeSize(avatarSize: number): 'sm' | 'md' | 'lg' {
    if (avatarSize <= 32) return 'sm';
    if (avatarSize <= 48) return 'md';
    return 'lg';
  }

  let badgeSize = $derived(getBadgeSize(size));
  let hasPicture = $derived(profile?.picture && !imgError);
  // Request 2x size for retina displays
  let proxySize = $derived(size * 2);
</script>

{#if showBadge}
  <div class="relative inline-block">
    {#if hasPicture}
      <ProxyImg
        src={profile!.picture}
        alt={name}
        width={proxySize}
        height={proxySize}
        square={true}
        class="rounded-full object-cover {className}"
        onerror={() => (imgError = true)}
        style="width: {size}px; height: {size}px;"
      />
    {:else}
      <div title={name}>
        <Minidenticon seed={pubkey} {size} class={className} />
      </div>
    {/if}
    <Badge
      pubKeyHex={pubkey}
      size={badgeSize}
      class="absolute -top-0.5 -right-0.5"
    />
  </div>
{:else if hasPicture}
  <ProxyImg
    src={profile!.picture}
    alt={name}
    width={proxySize}
    height={proxySize}
    square={true}
    class="rounded-full object-cover {className}"
    onerror={() => (imgError = true)}
    style="width: {size}px; height: {size}px;"
  />
{:else}
  <div title={name}>
    <Minidenticon seed={pubkey} {size} class={className} />
  </div>
{/if}
