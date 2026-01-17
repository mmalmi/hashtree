<script lang="ts">
  import { onMount } from 'svelte';
  import FileBrowser from '../components/FileBrowser.svelte';
  import Viewer from '../components/Viewer/Viewer.svelte';
  import { nostrStore } from '../nostr';
  import { isViewingFileStore, currentHash, currentDirCidStore, createGitInfoStore } from '../stores';
  import { nhashDecode } from 'hashtree';
  import { getQueryParamsFromHash } from '../lib/router.svelte';

  interface Props {
    nhash: string;
  }

  let { nhash }: Props = $props();

  let hash = $derived($currentHash);
  let isViewingFile = $derived($isViewingFileStore);
  let isValid = $state(true);

  // Check if current directory is a git repo
  let currentDirCid = $derived($currentDirCidStore);
  let gitInfoStore = $derived(createGitInfoStore(currentDirCid));
  let gitInfo = $derived($gitInfoStore);
  let isGitRepo = $derived(gitInfo.isRepo);

  // In single-column layout, show viewer for git repos or files
  let showViewer = $derived(isViewingFile || isGitRepo);

  // Check if fullscreen mode from URL
  let isFullscreen = $derived.by(() => {
    return getQueryParamsFromHash(hash).get('fullscreen') === '1';
  });

  onMount(() => {
    nostrStore.setSelectedTree(null);

    try {
      nhashDecode(nhash); // Validate
      isValid = true;
    } catch {
      isValid = false;
    }
  });
</script>

{#if isValid}
  <!-- File browser - hidden in single-column when viewing file/git repo, hidden completely in fullscreen -->
  {#if !isFullscreen}
    <div class={showViewer
      ? 'hidden lg:flex lg:w-80 shrink-0 flex-col min-h-0'
      : 'flex flex-1 lg:flex-none lg:w-80 shrink-0 flex-col min-h-0'}>
      <FileBrowser />
    </div>
  {/if}
  <!-- Viewer - shown in single-column when viewing file/git repo -->
  <div class={showViewer || isFullscreen
    ? 'flex flex-1 flex-col min-w-0 min-h-0 bg-surface-1/30'
    : 'hidden lg:flex flex-1 flex-col min-w-0 min-h-0 bg-surface-1/30'}>
    <Viewer />
  </div>
{:else}
  <div class="p-4 text-muted">Invalid nhash format</div>
{/if}
