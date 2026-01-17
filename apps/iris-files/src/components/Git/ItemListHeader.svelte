<script lang="ts">
  /**
   * Header for PR/Issue lists with filter tabs
   */
  import type { ItemStatus } from '../../nip34';
  import Dropdown from '../ui/Dropdown.svelte';

  interface Props {
    type: 'pr' | 'issue';
    counts: Record<ItemStatus | 'all', number>;
    filter: ItemStatus | 'all';
    onFilterChange: (filter: ItemStatus | 'all') => void;
    onNew?: () => void;
    canCreate: boolean;
  }

  let { type, counts, filter, onFilterChange, onNew, canCreate }: Props = $props();

  let isDropdownOpen = $state(false);

  const filterOptions = $derived.by(() => {
    const options: Array<{ value: ItemStatus | 'all'; label: string; icon: string }> = [
      { value: 'all', label: 'All', icon: 'i-lucide-list' },
      { value: 'open', label: 'Open', icon: type === 'pr' ? 'i-lucide-git-pull-request' : 'i-lucide-circle-dot' },
      { value: 'closed', label: 'Closed', icon: type === 'pr' ? 'i-lucide-git-pull-request-closed' : 'i-lucide-circle-x' },
    ];
    if (type === 'pr') {
      options.splice(2, 0, { value: 'merged', label: 'Merged', icon: 'i-lucide-git-merge' });
    }
    return options;
  });

  let selectedOption = $derived(filterOptions.find(o => o.value === filter) || filterOptions[0]);
</script>

<div class="flex items-center justify-between px-4 py-3 bg-surface-1 b-b-1 b-b-solid b-b-surface-3">
  <div class="flex items-center gap-4">
    <!-- Filter dropdown -->
    <Dropdown bind:open={isDropdownOpen} onClose={() => isDropdownOpen = false}>
      {#snippet trigger()}
        <button
          onclick={() => isDropdownOpen = !isDropdownOpen}
          class="btn-ghost flex items-center gap-2 px-3 h-9 text-sm"
        >
          <span class="{selectedOption.icon}"></span>
          {selectedOption.label}
          <span class="text-text-3">({counts[filter]})</span>
          <span class="i-lucide-chevron-down text-xs"></span>
        </button>
      {/snippet}
      {#each filterOptions as option (option.value)}
        <button
          onclick={() => { onFilterChange(option.value); isDropdownOpen = false; }}
          class="w-full text-left px-3 py-2 text-sm bg-surface-2 hover:bg-surface-3 flex items-center gap-2 text-text-1 b-0"
        >
          <span class="{option.icon} {filter === option.value ? 'text-accent' : 'text-text-2'}"></span>
          <span class={filter === option.value ? 'text-accent font-medium' : ''}>{option.label}</span>
          <span class="ml-auto text-text-3">{counts[option.value]}</span>
        </button>
      {/each}
    </Dropdown>
  </div>

  {#if canCreate && onNew}
    <button
      onclick={onNew}
      class="btn-primary flex items-center gap-2 px-3 h-9 text-sm"
    >
      <span class="i-lucide-plus"></span>
      New {type === 'pr' ? 'Pull Request' : 'Issue'}
    </button>
  {/if}
</div>
