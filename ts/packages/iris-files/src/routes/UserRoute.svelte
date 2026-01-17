<script lang="ts">
  import FileBrowser from '../components/FileBrowser.svelte';
  import ProfileView from '../components/ProfileView.svelte';
  import NHashRoute from './NHashRoute.svelte';
  import NPathRoute from './NPathRoute.svelte';
  import { nostrStore } from '../nostr';
  import { isNHash, isNPath } from 'hashtree';

  interface Props {
    id?: string;
    wild?: string;
  }

  let { id }: Props = $props();

  // Clear selected tree when viewing a user (not nhash/npath)
  $effect(() => {
    if (id && !isNHash(id) && !isNPath(id)) {
      nostrStore.setSelectedTree(null);
    }
  });
</script>

{#if id && isNHash(id)}
  <NHashRoute nhash={id} />
{:else if id && isNPath(id)}
  <NPathRoute npath={id} />
{:else}
  <!-- User view - Desktop: side-by-side layout -->
  <div class="hidden lg:flex lg:w-80 shrink-0 flex-col min-h-0">
    <FileBrowser />
  </div>
  <div class="hidden lg:flex flex-1 flex-col min-w-0 min-h-0 bg-surface-1/30">
    <ProfileView npub={id || ''} />
  </div>

  <!-- Mobile: stacked layout (profile on top, folders below) -->
  <div class="lg:hidden flex-1 overflow-y-auto">
    <ProfileView npub={id || ''} />
    <div class="border-t border-surface-2">
      <FileBrowser />
    </div>
  </div>
{/if}
