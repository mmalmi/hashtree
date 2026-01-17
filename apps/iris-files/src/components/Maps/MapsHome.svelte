<script lang="ts">
  /**
   * MapsHome - Map view for maps.iris.to
   */
  import { onMount } from 'svelte';
  import L from 'leaflet';
  import MapControls from './MapControls.svelte';
  import AddPlaceModal from './AddPlaceModal.svelte';
  import { mapsStore } from '../../stores/mapsStore.svelte';
  import type { ActivePlace } from '../../stores/mapsStore.svelte';
  import { currentHash } from '../../stores/route';
  import { parseActivePlaceFromHash, setActivePlaceInHash } from '../../lib/mapsUrl';

  // Fix Leaflet default marker icon issue with bundlers
  import markerIcon2x from 'leaflet/dist/images/marker-icon-2x.png';
  import markerIcon from 'leaflet/dist/images/marker-icon.png';
  import markerShadow from 'leaflet/dist/images/marker-shadow.png';

  // @ts-expect-error - Leaflet icon fix
  delete L.Icon.Default.prototype._getIconUrl;
  L.Icon.Default.mergeOptions({
    iconUrl: markerIcon,
    iconRetinaUrl: markerIcon2x,
    shadowUrl: markerShadow,
  });

  let mapContainer: HTMLDivElement;
  let map: L.Map | null = null;
  let tileLayer: L.TileLayer | null = null;
  let placesLayer: L.LayerGroup | null = null;
  let activePlaceMarker: L.Marker | null = null;
  let mapReady = $state(false);

  let places = $derived(mapsStore.places);
  let activePlace = $derived(mapsStore.activePlace);
  let lastFocusedKey = '';

  const largeAreaTypes = new Set(['country', 'state', 'region', 'province', 'county', 'administrative']);
  const largeAreaClasses = new Set(['boundary']);

  function shouldShowActivePlaceMarker(place: ActivePlace): boolean {
    if (place.type && largeAreaTypes.has(place.type.toLowerCase())) {
      return false;
    }
    if (place.class && largeAreaClasses.has(place.class.toLowerCase())) {
      return false;
    }
    if (typeof place.placeRank === 'number' && place.placeRank <= 6) {
      return false;
    }
    if (place.boundingBox) {
      const [south, north, west, east] = place.boundingBox;
      const latSpan = north - south;
      const lngSpan = Math.abs(east - west);
      return latSpan <= 20 && lngSpan <= 20;
    }
    return true;
  }

  function focusActivePlace(place: ActivePlace) {
    if (!map) return;
    if (place.boundingBox) {
      const [south, north, west, east] = place.boundingBox;
      const latSpan = north - south;
      const lngSpan = Math.abs(east - west);
      if (latSpan > 20 || lngSpan > 20) {
        map.setView([place.lat, place.lng], 4);
      } else {
        map.fitBounds([[south, west], [north, east]], { maxZoom: 16, padding: [20, 20] });
      }
    } else {
      map.setView([place.lat, place.lng], 15);
    }
  }

  function syncActivePlaceFromHash(hash: string) {
    const nextPlace = parseActivePlaceFromHash(hash);
    if (!nextPlace) {
      if (mapsStore.activePlace) {
        mapsStore.setActivePlace(null);
      }
      return;
    }
    const current = mapsStore.activePlace;
    if (
      !current ||
      current.lat !== nextPlace.lat ||
      current.lng !== nextPlace.lng ||
      current.name !== nextPlace.name
    ) {
      mapsStore.setActivePlace(nextPlace);
    }
  }

  $effect(() => {
    if (!mapReady || !map || !placesLayer) return;
    placesLayer.clearLayers();
    for (const place of places) {
      const marker = L.marker([place.lat, place.lng])
        .bindPopup(`<strong>${place.name}</strong><br/>${place.category}`);
      placesLayer.addLayer(marker);
    }
  });

  $effect(() => {
    if (!mapReady || !map) return;
    if (activePlaceMarker) {
      map.removeLayer(activePlaceMarker);
      activePlaceMarker = null;
    }
    if (!activePlace || !shouldShowActivePlaceMarker(activePlace)) return;
    activePlaceMarker = L.marker([activePlace.lat, activePlace.lng]);
    activePlaceMarker.addTo(map);
  });

  $effect(() => {
    if (!mapReady || !map || !activePlace) {
      lastFocusedKey = '';
      return;
    }
    const nextKey = `${activePlace.name}-${activePlace.lat}-${activePlace.lng}`;
    if (nextKey === lastFocusedKey) return;
    lastFocusedKey = nextKey;
    focusActivePlace(activePlace);
  });

  onMount(() => {
    mapsStore.init();

    if (mapContainer) {
      const savedView = mapsStore.mapView;
      map = L.map(mapContainer, { attributionControl: false, zoomControl: false }).setView(savedView.center, savedView.zoom);
      L.control.attribution({ prefix: false }).addTo(map);
      L.control.zoom({ position: 'bottomright' }).addTo(map);

      tileLayer = L.tileLayer('https://tile.openstreetmap.org/{z}/{x}/{y}.png', {
        maxZoom: 19,
        attribution: '&copy; <a href="http://www.openstreetmap.org/copyright">OpenStreetMap</a>',
      });
      tileLayer.addTo(map);

      placesLayer = L.layerGroup().addTo(map);

      map.on('click', (e: L.LeafletMouseEvent) => {
        if (mapsStore.isAddPlaceModalOpen) {
          mapsStore.setPendingPlaceLocation(e.latlng.lat, e.latlng.lng);
        }
      });

      map.on('moveend', () => {
        if (map) {
          const center = map.getCenter();
          const bounds = map.getBounds();
          mapsStore.setMapView(
            [center.lat, center.lng],
            map.getZoom(),
            [bounds.getSouth(), bounds.getNorth(), bounds.getWest(), bounds.getEast()]
          );
        }
      });
      mapReady = true;
    }

    syncActivePlaceFromHash(window.location.hash);
    const unsubscribeHash = currentHash.subscribe((hash) => {
      syncActivePlaceFromHash(hash);
    });

    return () => {
      unsubscribeHash();
      mapsStore.destroy();
      if (map) {
        map.remove();
        map = null;
        tileLayer = null;
        placesLayer = null;
      }
      mapReady = false;
    };
  });
</script>

<div class="flex-1 relative">
  <div bind:this={mapContainer} class="absolute inset-0 bg-surface-0"></div>
  <MapControls />
  {#if activePlace && !mapsStore.isAddPlaceModalOpen}
    <aside class="fixed top-16 left-4 z-[10000] bg-surface-1 rounded-lg shadow-xl w-80 p-4 max-h-[calc(100vh-5rem)] overflow-y-auto b-1 b-solid b-surface-3">
      <div class="flex items-start justify-between gap-2 mb-2">
        <div class="min-w-0">
          <h2 class="text-lg font-semibold truncate">{activePlace.name}</h2>
          {#if activePlace.displayName}
            <p class="text-xs text-text-3 truncate">{activePlace.displayName}</p>
          {/if}
        </div>
        <button type="button" class="text-text-2 hover:text-text-1" title="Clear" onclick={() => {
          mapsStore.setActivePlace(null);
          setActivePlaceInHash(null);
        }}>
          <span class="i-lucide-x text-lg"></span>
        </button>
      </div>
      <div class="text-sm text-text-2 space-y-2">
        {#if activePlace.type}
          <div class="flex items-center gap-2">
            <span class="i-lucide-tag text-base text-text-3"></span>
            <span class="capitalize">{activePlace.type.replace(/_/g, ' ')}</span>
          </div>
        {/if}
        <div class="flex items-center gap-2">
          <span class="i-lucide-map-pin text-base text-text-3"></span>
          <span>{activePlace.lat.toFixed(5)}, {activePlace.lng.toFixed(5)}</span>
        </div>
      </div>
    </aside>
  {/if}
</div>

<AddPlaceModal />

<style>
  :global(.leaflet-container) {
    background: var(--surface-0);
  }
</style>
