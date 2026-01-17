<script lang="ts">
  import Avatar from './Avatar.svelte';
  import Name from './Name.svelte';

  interface Props {
    pubkey: string;
    description?: string;
    avatarSize?: number;
    showBadge?: boolean;
    class?: string;
  }

  let { pubkey, description, avatarSize = 36, showBadge = true, class: className = '' }: Props = $props();
</script>

<div class="flex items-center gap-3 {className}">
  <Avatar {pubkey} size={avatarSize} {showBadge} />
  <div class="flex flex-col min-w-0">
    <Name {pubkey} class="text-sm" />
    {#if description}
      <span class="text-xs text-muted truncate">{description}</span>
    {/if}
  </div>
</div>
