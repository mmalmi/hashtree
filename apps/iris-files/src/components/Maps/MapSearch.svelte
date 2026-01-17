<script lang="ts">
  import { onMount } from 'svelte';
  import { mapsStore, getSearchResultKey } from '../../stores/mapsStore.svelte';

  let { onSelect }: { onSelect: (result: typeof mapsStore.searchResults[0]) => void } = $props();

  let inputValue = $state('');
  let focused = $state(false);
  let selectedIndex = $state(0);
  let containerRef: HTMLDivElement | undefined = $state();
  let dropdownRef: HTMLDivElement | undefined = $state();
  let debounceTimer: ReturnType<typeof setTimeout> | null = null;

  function updateDropdownPosition() {
    if (containerRef && dropdownRef) {
      const rect = containerRef.getBoundingClientRect();
      dropdownRef.style.top = `${rect.bottom + 4}px`;
      dropdownRef.style.left = `${rect.left}px`;
      dropdownRef.style.width = `${rect.width}px`;
    }
  }

  // Portal dropdown to body to escape header's stacking context
  onMount(() => {
    if (dropdownRef) {
      document.body.appendChild(dropdownRef);
    }
    return () => {
      if (dropdownRef && dropdownRef.parentNode === document.body) {
        document.body.removeChild(dropdownRef);
      }
    };
  });

  // Update position when results change
  $effect(() => {
    if (mapsStore.searchResults.length > 0 || mapsStore.isSearching) {
      updateDropdownPosition();
    }
  });

  function handleInput(e: Event) {
    const value = (e.target as HTMLInputElement).value;
    inputValue = value;

    if (debounceTimer) clearTimeout(debounceTimer);
    debounceTimer = setTimeout(() => {
      mapsStore.search(value);
      selectedIndex = 0;
      updateDropdownPosition();
    }, 300);
  }

  function handleSelect(result: typeof mapsStore.searchResults[0]) {
    onSelect(result);
    inputValue = '';
    mapsStore.clearSearch();
  }

  function handleClear() {
    inputValue = '';
    mapsStore.clearSearch();
  }

  function handleKeydown(e: KeyboardEvent) {
    const results = mapsStore.searchResults;
    if (results.length > 0) {
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        selectedIndex = (selectedIndex + 1) % results.length;
        return;
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault();
        selectedIndex = (selectedIndex - 1 + results.length) % results.length;
        return;
      }
      if (e.key === 'Enter' && results[selectedIndex]) {
        e.preventDefault();
        handleSelect(results[selectedIndex]);
        return;
      }
    }
    if (e.key === 'Escape') {
      mapsStore.clearSearch();
    }
  }

  function handleFocus() {
    focused = true;
    updateDropdownPosition();
  }
</script>

<div bind:this={containerRef} class="relative w-full max-w-lg" data-testid="map-search">
  <div class="flex items-center gap-2 px-4 py-2 rounded-full bg-surface-0 b-1 b-solid b-surface-3 transition-colors {focused ? 'b-accent' : ''}">
    <span class="i-lucide-search text-sm text-muted shrink-0"></span>
    <input
      type="text"
      placeholder="Search location"
      value={inputValue}
      oninput={handleInput}
      onkeydown={handleKeydown}
      onfocus={handleFocus}
      onblur={() => focused = false}
      class="bg-transparent border-none outline-none text-base text-text-1 placeholder:text-muted flex-1"
      data-testid="search-input"
    />
    {#if inputValue}
      <button
        type="button"
        onclick={handleClear}
        class="text-muted hover:text-text-1 shrink-0"
        title="Clear"
      >
        <span class="i-lucide-x text-sm"></span>
      </button>
    {/if}
  </div>
</div>

<div
  bind:this={dropdownRef}
  class="fixed bg-surface-1 rounded-lg shadow-lg z-[9999] max-h-80 overflow-auto"
  class:hidden={mapsStore.searchResults.length === 0 && !mapsStore.isSearching}
  data-testid="search-results"
>
  {#if mapsStore.isSearching && inputValue}
    <div class="p-3 text-muted text-center text-sm">
      Searching...
    </div>
  {:else}
    {#each mapsStore.searchResults as result, index (getSearchResultKey(result))}
      <button
        type="button"
        onclick={() => handleSelect(result)}
        onmouseenter={() => selectedIndex = index}
        class="w-full text-left flex items-center gap-3 px-3 py-2 {index === selectedIndex ? 'bg-surface-3' : 'hover:bg-surface-2'}"
      >
        <span class="i-lucide-map-pin text-lg text-text-3 shrink-0"></span>
        <div class="min-w-0 flex-1">
          <div class="text-sm text-text-1 truncate">{result.name}</div>
          <div class="text-xs text-text-3 truncate">{result.displayName}</div>
        </div>
      </button>
    {/each}
  {/if}
</div>
