<script lang="ts">
  import { mapsStore, type PlaceCategory } from '../../stores/mapsStore.svelte';
  import { nostrStore } from '../../nostr';
  import { get } from 'svelte/store';

  let name = $state('');
  let category = $state<PlaceCategory>('restaurant');
  let description = $state('');
  let contact = $state('');
  let hours = $state('');

  const categories: { value: PlaceCategory; label: string }[] = [
    { value: 'restaurant', label: 'Restaurant' },
    { value: 'cafe', label: 'Cafe' },
    { value: 'bar', label: 'Bar' },
    { value: 'shop', label: 'Shop' },
    { value: 'service', label: 'Service' },
    { value: 'attraction', label: 'Attraction' },
    { value: 'accommodation', label: 'Accommodation' },
    { value: 'transport', label: 'Transport' },
    { value: 'other', label: 'Other' },
  ];

  function handleClose() {
    mapsStore.closeAddPlaceModal();
    resetForm();
  }

  function resetForm() {
    name = '';
    category = 'restaurant';
    description = '';
    contact = '';
    hours = '';
  }

  async function handleSave() {
    const location = mapsStore.pendingPlaceLocation;
    if (!name.trim() || !location) return;

    await mapsStore.addPlace({
      name: name.trim(),
      category,
      lat: location.lat,
      lng: location.lng,
      description: description.trim() || undefined,
      contact: contact.trim() || undefined,
      hours: hours.trim() || undefined,
      owner: get(nostrStore).npub || 'anonymous',
    });

    handleClose();
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      handleClose();
    }
  }
</script>

<svelte:window onkeydown={handleKeydown} />

{#if mapsStore.isAddPlaceModalOpen}
  <!-- Side panel that doesn't block map interaction -->
  <div
    class="fixed top-16 left-4 z-[10001] bg-surface-1 rounded-lg shadow-xl w-80 p-4 max-h-[calc(100vh-5rem)] overflow-y-auto"
    role="dialog"
    aria-modal="false"
    aria-labelledby="modal-title"
  >
    <div class="flex justify-between items-center mb-4">
      <h2 id="modal-title" class="text-xl font-semibold">Add Place</h2>
      <button type="button" onclick={handleClose} class="text-text-2 hover:text-text-1" title="Close">
        <span class="i-lucide-x text-xl"></span>
      </button>
    </div>

    <div class="space-y-3">
      <div>
        <label for="place-name" class="block text-sm font-medium mb-1">Name</label>
        <input
          id="place-name"
          type="text"
          placeholder="Place name..."
          bind:value={name}
          class="w-full px-3 py-2 bg-surface-2 rounded b-1 b-border-1 focus:b-accent outline-none"
        />
      </div>

      <div>
        <label for="place-category" class="block text-sm font-medium mb-1">Category</label>
        <select
          id="place-category"
          name="category"
          bind:value={category}
          class="w-full px-3 py-2 bg-surface-2 rounded b-1 b-border-1 focus:b-accent outline-none"
        >
          {#each categories as cat (cat.value)}
            <option value={cat.value}>{cat.label}</option>
          {/each}
        </select>
      </div>

      <div>
        <label for="place-description" class="block text-sm font-medium mb-1">Description</label>
        <textarea
          id="place-description"
          placeholder="Optional description..."
          bind:value={description}
          rows="2"
          class="w-full px-3 py-2 bg-surface-2 rounded b-1 b-border-1 focus:b-accent outline-none resize-none"
        ></textarea>
      </div>

      <div>
        <label for="place-contact" class="block text-sm font-medium mb-1">Contact</label>
        <input
          id="place-contact"
          type="text"
          placeholder="Phone, email, website..."
          bind:value={contact}
          class="w-full px-3 py-2 bg-surface-2 rounded b-1 b-border-1 focus:b-accent outline-none"
        />
      </div>

      <div>
        <label for="place-hours" class="block text-sm font-medium mb-1">Hours</label>
        <input
          id="place-hours"
          type="text"
          placeholder="e.g., Mon-Fri 9am-5pm"
          bind:value={hours}
          class="w-full px-3 py-2 bg-surface-2 rounded b-1 b-border-1 focus:b-accent outline-none"
        />
      </div>

      {#if !mapsStore.pendingPlaceLocation}
        <p class="text-sm text-text-2">Click on the map to set the location.</p>
      {:else}
        <p class="text-sm text-text-2">
          Location: {mapsStore.pendingPlaceLocation.lat.toFixed(5)}, {mapsStore.pendingPlaceLocation.lng.toFixed(5)}
        </p>
      {/if}
    </div>

    <div class="flex justify-end gap-3 mt-4">
      <button
        type="button"
        onclick={handleClose}
        class="px-4 py-2 rounded btn-ghost"
      >
        Cancel
      </button>
      <button
        type="button"
        onclick={handleSave}
        disabled={!name.trim() || !mapsStore.pendingPlaceLocation}
        class="px-4 py-2 rounded btn-primary disabled:opacity-50 disabled:cursor-not-allowed"
      >
        Save
      </button>
    </div>
  </div>
{/if}
