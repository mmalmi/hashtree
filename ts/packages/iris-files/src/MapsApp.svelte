<script lang="ts">
  /**
   * MapsApp - Map-focused app shell for maps.iris.to
   * OpenStreetMap-based maps with offline-first sync via HashTree
   */
  import { onMount } from 'svelte';
  import Logo from './components/Logo.svelte';
  import Header from './components/Header.svelte';
  import NostrLogin from './components/NostrLogin.svelte';
  import ConnectivityIndicator from './components/ConnectivityIndicator.svelte';
  import BandwidthIndicator from './components/BandwidthIndicator.svelte';
  import { settingsStore } from './stores/settings';
  import Toast from './components/Toast.svelte';
  import { currentPath, initRouter } from './lib/router.svelte';
  import MapSearch from './components/Maps/MapSearch.svelte';
  import { mapsStore } from './stores/mapsStore.svelte';
  import type { SearchResult } from './stores/mapsStore.svelte';
  import { setActivePlaceInHash } from './lib/mapsUrl';
  import MapsRouter from './components/Maps/MapsRouter.svelte';

  const isDev = import.meta.env.DEV;
  let RenderScan: typeof import('svelte-render-scan').RenderScan | null = $state(null);
  if (isDev) {
    import('svelte-render-scan').then(m => RenderScan = m.RenderScan);
  }

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

  let currentPathValue = $derived($currentPath);
  let showMapSearch = $derived(currentPathValue === '/');

  function goToLocation(result: SearchResult) {
    mapsStore.setActivePlace({
      name: result.name,
      lat: result.lat,
      lng: result.lng,
      type: result.type,
      class: result.class,
      displayName: result.displayName,
      boundingBox: result.boundingBox,
      placeRank: result.placeRank,
      source: 'search',
    });
    setActivePlaceInHash(result);
  }
</script>

<div class="h-full flex flex-col bg-surface-0">
  <Header>
    <div class="flex items-center shrink-0">
      <a href="#/" class="no-underline select-none" onclick={handleLogoClick}>
        <Logo app="maps" />
      </a>
    </div>
    <div class="flex-1 flex justify-center px-4 overflow-visible">
      {#if showMapSearch}
        <MapSearch onSelect={goToLocation} />
      {/if}
    </div>
    <div class="flex items-center gap-2 md:gap-3 shrink-0">
      {#if showBandwidth}
        <BandwidthIndicator />
      {/if}
      {#if showConnectivity}
        <ConnectivityIndicator />
      {/if}
      <NostrLogin />
    </div>
  </Header>

  <div class="flex-1 flex flex-col">
    <MapsRouter currentPath={$currentPath} />
  </div>

  <!-- Modals -->
  <Toast />
  {#if RenderScan}
    <RenderScan initialEnabled={false} />
  {/if}
</div>
