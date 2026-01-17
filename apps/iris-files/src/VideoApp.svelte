<script lang="ts">
  /**
   * VideoApp - Video-focused app shell for video.iris.to
   * YouTube-style UI for video sharing and streaming
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
  import WalletLink from './components/WalletLink.svelte';
  import Toast from './components/Toast.svelte';
  import VideoRouter from './components/Video/VideoRouter.svelte';
  import Dropdown from './components/ui/Dropdown.svelte';
  import { currentPath, initRouter } from './lib/router.svelte';
  import { nostrStore } from './nostr';

  const isDev = import.meta.env.DEV;
  let RenderScan: typeof import('svelte-render-scan').RenderScan | null = $state(null);
  if (isDev) {
    import('svelte-render-scan').then(m => RenderScan = m.RenderScan);
  }

  // Modal components
  import ShareModal from './components/Modals/ShareModal.svelte';
  import ForkModal from './components/Modals/ForkModal.svelte';
  import BlossomPushModal from './components/Modals/BlossomPushModal.svelte';
  import AddToPlaylistModal from './components/Modals/AddToPlaylistModal.svelte';
  import ZapModal from './components/Modals/ZapModal.svelte';
  import VideoUploadModal, { open as openVideoUploadModal } from './components/Video/VideoUploadModal.svelte';
  import ImportModal, { open as openImportModal } from './components/Video/ImportModal.svelte';

  let isLoggedIn = $derived($nostrStore.isLoggedIn);
  let createDropdownOpen = $state(false);
  let showBandwidth = $derived($settingsStore.pools.showBandwidth ?? false);
  let showConnectivity = $derived($settingsStore.pools.showConnectivity ?? true);

  onMount(() => {
    initRouter();
  });

  function handleUploadVideo() {
    createDropdownOpen = false;
    openVideoUploadModal();
  }

  function handleLivestream() {
    createDropdownOpen = false;
    window.location.hash = '#/create';
  }

  function handleImport() {
    createDropdownOpen = false;
    openImportModal();
  }

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
        <Logo app="video" />
      </a>
    </div>
    <div class="flex-1 hidden md:flex justify-center px-4">
      <SearchInput placeholder="Search videos" />
    </div>
    <div class="flex-1 md:hidden"></div>
    <div class="flex items-center gap-2 md:gap-3 shrink-0">
      <MobileSearch />
      {#if isLoggedIn}
        <Dropdown bind:open={createDropdownOpen} onClose={() => createDropdownOpen = false} align="right">
          {#snippet trigger()}
            <button
              onclick={() => createDropdownOpen = !createDropdownOpen}
              class="btn-ghost px-3 py-2 flex items-center gap-1 max-sm:btn-circle max-sm:p-0"
              title="Create"
            >
              <span class="i-lucide-plus text-lg"></span>
              <span class="hidden sm:inline text-sm">Create</span>
            </button>
          {/snippet}
          <div class="bg-surface-2 rounded-lg overflow-hidden">
            <button onclick={handleUploadVideo} class="w-full px-4 py-2 text-left btn-ghost rounded-none flex items-center gap-3">
              <span class="i-lucide-upload text-lg"></span>
              <span>Upload Video</span>
            </button>
            <button onclick={handleLivestream} class="w-full px-4 py-2 text-left btn-ghost rounded-none flex items-center gap-3">
              <span class="i-lucide-radio text-lg"></span>
              <span>Livestream</span>
            </button>
            <button onclick={handleImport} class="w-full px-4 py-2 text-left btn-ghost rounded-none flex items-center gap-3">
              <span class="i-lucide-folder-input text-lg"></span>
              <span>Import</span>
            </button>
          </div>
        </Dropdown>
      {/if}
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
    <VideoRouter currentPath={$currentPath} />
  </div>

  <!-- Modals -->
  <ShareModal />
  <ForkModal />
  <BlossomPushModal />
  <AddToPlaylistModal />
  <ZapModal />
  <VideoUploadModal />
  <ImportModal />
  <Toast />
  {#if RenderScan}
    <RenderScan initialEnabled={false} />
  {/if}
</div>
