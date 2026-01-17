<script lang="ts">
  /**
   * Displays author with avatar, name, and social graph badge
   * Links to user profile
   */
  import Avatar from '../User/Avatar.svelte';
  import Name from '../User/Name.svelte';
  import { nip19 } from 'nostr-tools';

  interface Props {
    pubkey: string;
    npub?: string;
    size?: 'sm' | 'md';
  }

  let { pubkey, npub, size = 'sm' }: Props = $props();

  let avatarSize = $derived(size === 'sm' ? 20 : 28);
  let userNpub = $derived(npub || (pubkey ? nip19.npubEncode(pubkey) : ''));
</script>

<a href="#/{userNpub}" class="inline-flex items-center gap-1.5 hover:opacity-80">
  <Avatar {pubkey} size={avatarSize} showBadge={true} />
  <Name {pubkey} class="text-text-2 hover:text-accent hover:underline" />
</a>
