<script lang="ts">
  /**
   * DocsApp - Simplified document-focused app shell for docs.iris.to
   * Google Docs-style UI focused on collaborative documents
   */
  import { onMount } from 'svelte';
  import Logo from './components/Logo.svelte';
  import Header from './components/Header.svelte';
  import NostrLogin from './components/NostrLogin.svelte';
  import ConnectivityIndicator from './components/ConnectivityIndicator.svelte';
  import BandwidthIndicator from './components/BandwidthIndicator.svelte';
  import SearchInput from './components/SearchInput.svelte';
  import { settingsStore } from './stores/settings';
  import MobileSearch from './components/MobileSearch.svelte';
  import Toast from './components/Toast.svelte';
  import DocsRouter from './components/Docs/DocsRouter.svelte';
  import { currentPath, initRouter } from './lib/router.svelte';

  // Modal components
  import ShareModal from './components/Modals/ShareModal.svelte';
  import CollaboratorsModal from './components/Modals/CollaboratorsModal.svelte';
  import ForkModal from './components/Modals/ForkModal.svelte';
  import BlossomPushModal from './components/Modals/BlossomPushModal.svelte';
  import CreateModal from './components/Modals/CreateModal.svelte';

  // Header display settings
  let showBandwidth = $derived($settingsStore.pools.showBandwidth ?? false);
  let showConnectivity = $derived($settingsStore.pools.showConnectivity ?? true);

  onMount(() => {
    initRouter();
  });

  function handleLogoClick(e: MouseEvent) {
    // If already on home page, scroll to top instead of navigating
    if (window.location.hash === '#/' || window.location.hash === '' || window.location.hash === '#') {
      e.preventDefault();
      window.scrollTo({ top: 0, behavior: 'smooth' });
    }
  }
</script>

<div class="h-full flex flex-col bg-surface-0">
  <Header>
    <div class="flex items-center shrink-0">
      <a href="#/" class="no-underline select-none" onclick={handleLogoClick}>
        <Logo app="docs" />
      </a>
    </div>
    <div class="flex-1 hidden md:flex justify-center px-4">
      <SearchInput showVideos={false} />
    </div>
    <div class="flex-1 md:hidden"></div>
    <div class="flex items-center gap-2 md:gap-3 shrink-0">
      <MobileSearch showVideos={false} />
      {#if showBandwidth}
        <BandwidthIndicator />
      {/if}
      {#if showConnectivity}
        <ConnectivityIndicator />
      {/if}
      <NostrLogin />
    </div>
  </Header>

  <!-- Main area -->
  <div class="flex-1 flex flex-col">
    <DocsRouter currentPath={$currentPath} />
  </div>

  <!-- Modals -->
  <CreateModal />
  <ShareModal />
  <CollaboratorsModal />
  <ForkModal />
  <BlossomPushModal />
  <Toast />
</div>
