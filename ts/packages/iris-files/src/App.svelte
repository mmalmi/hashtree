<script lang="ts">
  import { onMount } from 'svelte';
  import Logo from './components/Logo.svelte';
  import Header from './components/Header.svelte';
  import NostrLogin from './components/NostrLogin.svelte';
  import ConnectivityIndicator from './components/ConnectivityIndicator.svelte';
  import BandwidthIndicator from './components/BandwidthIndicator.svelte';
  import SearchInput from './components/SearchInput.svelte';
  import MobileSearch from './components/MobileSearch.svelte';
  import WalletLink from './components/WalletLink.svelte';
  import Toast from './components/Toast.svelte';
  import Router from './components/Router.svelte';
  import { currentPath, initRouter, getQueryParams } from './lib/router.svelte';
  import { settingsStore } from './stores/settings';

  const isDev = import.meta.env.DEV;
  let RenderScan: typeof import('svelte-render-scan').RenderScan | null = $state(null);
  if (isDev) {
    import('svelte-render-scan').then(m => RenderScan = m.RenderScan);
  }

  // Modal components
  import CreateModal from './components/Modals/CreateModal.svelte';
  import RenameModal from './components/Modals/RenameModal.svelte';
  import ForkModal from './components/Modals/ForkModal.svelte';
  import ExtractModal from './components/Modals/ExtractModal.svelte';
  import GitignoreModal from './components/Modals/GitignoreModal.svelte';
  import GitHistoryModal from './components/Modals/GitHistoryModal.svelte';
  import GitShellModal from './components/Modals/GitShellModal.svelte';
  import GitCommitModal from './components/Modals/GitCommitModal.svelte';
  import CIRunsModal from './components/Modals/CIRunsModal.svelte';
  import ShareModal from './components/Modals/ShareModal.svelte';
  import CollaboratorsModal from './components/Modals/CollaboratorsModal.svelte';
  import UnsavedChangesModal from './components/Modals/UnsavedChangesModal.svelte';
  import NewPullRequestModal from './components/Git/NewPullRequestModal.svelte';
  import NewIssueModal from './components/Git/NewIssueModal.svelte';
  import BlossomPushModal from './components/Modals/BlossomPushModal.svelte';

  // Handle fullscreen mode from URL
  function isFullscreen(): boolean {
    const params = getQueryParams();
    return params.get('fullscreen') === '1';
  }

  function clearFullscreen() {
    const hash = window.location.hash.split('?')[0];
    const params = getQueryParams();
    params.delete('fullscreen');
    const queryString = params.toString();
    window.location.hash = queryString ? `${hash}?${queryString}` : hash;
  }

  // Fullscreen state - check on each path change
  let fullscreen = $derived(isFullscreen());

  // Header display settings (default to true/false if not yet loaded)
  let showConnectivity = $derived($settingsStore.pools.showConnectivity ?? true);
  let showBandwidth = $derived($settingsStore.pools.showBandwidth ?? false);

  onMount(() => {
    initRouter();
  });

  function handleLogoClick(e: MouseEvent) {
    if (fullscreen) {
      e.preventDefault();
      clearFullscreen();
    } else if (window.location.hash === '#/' || window.location.hash === '' || window.location.hash === '#') {
      // If already on home page, scroll to top instead of navigating
      e.preventDefault();
      window.scrollTo({ top: 0, behavior: 'smooth' });
    }
  }
</script>

<div class="h-full flex flex-col bg-surface-0">
  <Header>
    <div class="flex items-center shrink-0">
      <a href="#/" onclick={handleLogoClick} class="no-underline">
        <Logo />
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
      <WalletLink />
      <NostrLogin />
    </div>
  </Header>

  <!-- Main area -->
  <div class="flex-1 flex flex-col">
    <Router currentPath={$currentPath} />
  </div>

  <!-- Modals -->
  <CreateModal />
  <RenameModal />
  <ForkModal />
  <ExtractModal />
  <GitignoreModal />
  <GitHistoryModal />
  <GitShellModal />
  <GitCommitModal />
  <CIRunsModal />
  <ShareModal />
  <CollaboratorsModal />
  <UnsavedChangesModal />
  <NewPullRequestModal />
  <NewIssueModal />
  <BlossomPushModal />
  <CIRunsModal />
  <Toast />
  {#if RenderScan}
    <RenderScan initialEnabled={false} />
  {/if}
</div>
