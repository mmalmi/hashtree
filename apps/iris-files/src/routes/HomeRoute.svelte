<script lang="ts">
  import { onMount } from 'svelte';
  import FileBrowser from '../components/FileBrowser.svelte';
  import RecentsView from '../components/RecentsView.svelte';
  import FollowsTreesView from '../components/FollowsTreesView.svelte';
  import { nostrStore } from '../nostr';

  onMount(() => {
    // Clear selected tree when on home route
    nostrStore.setSelectedTree(null);
  });
</script>

<!-- Home Route: Show FileBrowser on left, Recents+Follows on right (desktop) or just FileBrowser (mobile) -->
<div class="flex flex-1 lg:flex-none lg:w-80 shrink-0 flex-col min-h-0">
  <FileBrowser />
</div>
<div class="hidden lg:flex flex-1 flex-col min-w-0 min-h-0 bg-surface-1/30">
  <div class="flex flex-1 min-h-0">
    <div class="flex-1 flex flex-col min-w-0">
      <FollowsTreesView />
    </div>
    <div class="flex-1 flex flex-col min-w-0">
      <RecentsView />
    </div>
  </div>
</div>
