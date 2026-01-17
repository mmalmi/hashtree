<script lang="ts">
  import { mapsStore, type AudienceFilter } from '../../stores/mapsStore.svelte';

  function handleAddPlace() {
    mapsStore.openAddPlaceModal();
  }

  function handleFilterChange(e: Event) {
    const select = e.target as HTMLSelectElement;
    mapsStore.setAudienceFilter(select.value as AudienceFilter);
  }
</script>

<div class="absolute top-4 right-4 z-[10000] flex flex-col gap-2">
  <!-- Add Place Button -->
  <button
    type="button"
    title="Add Place"
    onclick={handleAddPlace}
    class="w-10 h-10 bg-surface-1 rounded-lg shadow-lg flex items-center justify-center hover:bg-surface-2 transition-colors b-1 b-border-1"
  >
    <span class="i-lucide-map-pin text-xl"></span>
  </button>

  <!-- Audience Filter -->
  <select
    data-testid="audience-filter"
    value={mapsStore.audienceFilter}
    onchange={handleFilterChange}
    class="px-2 py-2 bg-surface-1 rounded-lg shadow-lg b-1 b-border-1 text-sm outline-none focus:b-accent"
  >
    <option value="own">Own</option>
    <option value="follows">Follows</option>
    <option value="fof">Follows of Follows</option>
  </select>
</div>
