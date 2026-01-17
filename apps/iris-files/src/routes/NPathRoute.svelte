<script lang="ts">
  import { onMount } from 'svelte';
  import { push } from '../lib/router.svelte';
  import { npathDecode } from 'hashtree';
  import { nip19 } from 'nostr-tools';

  interface Props {
    npath: string;
  }

  let { npath }: Props = $props();

  onMount(() => {
    try {
      const decoded = npathDecode(npath);
      // Convert pubkey to npub
      const npub = nip19.npubEncode(decoded.pubkey);
      // Build URL path: /npub/treeName/path...
      const parts = [npub, decoded.treeName, ...decoded.path];
      const url = '/' + parts.map(encodeURIComponent).join('/');
      // Replace current history entry (don't add npath to history)
      push(url);
    } catch {
      // Invalid npath - navigate home
      push('/');
    }
  });
</script>

<!-- Brief render while redirecting -->
<div></div>
