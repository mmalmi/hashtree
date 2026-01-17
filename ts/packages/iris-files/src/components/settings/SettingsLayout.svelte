<script lang="ts">
  import { currentPath, navigate } from '../../lib/router.svelte';
  import ServersSettings from './ServersSettings.svelte';
  import P2PSettings from './P2PSettings.svelte';
  import StorageSettings from './StorageSettings.svelte';
  import AppSettings from './AppSettings.svelte';

  const tabs = [
    { id: 'servers', label: 'Servers', icon: 'i-lucide-server' },
    { id: 'p2p', label: 'P2P', icon: 'i-lucide-share-2' },
    { id: 'storage', label: 'Storage', icon: 'i-lucide-hard-drive' },
    { id: 'app', label: 'App', icon: 'i-lucide-settings' },
  ] as const;

  type TabId = (typeof tabs)[number]['id'];

  // Parse current tab from path
  let activeTab = $derived.by((): TabId => {
    const path = $currentPath;
    if (path.startsWith('/settings/p2p')) return 'p2p';
    if (path.startsWith('/settings/storage')) return 'storage';
    if (path.startsWith('/settings/app')) return 'app';
    return 'servers'; // default
  });

  function selectTab(id: TabId) {
    navigate(`/settings/${id}`);
  }
</script>

<div class="flex-1 flex flex-col overflow-hidden">
  <!-- Tab navigation -->
  <div class="shrink-0 flex border-b border-surface-2 bg-surface-1 px-4">
    {#each tabs as tab (tab.id)}
      <button
        onclick={() => selectTab(tab.id)}
        class="px-4 py-3 text-sm font-medium flex items-center gap-2 border-b-2 -mb-px transition-colors
          {activeTab === tab.id
            ? 'border-accent text-text-1'
            : 'border-transparent text-text-2 hover:text-text-1 hover:border-surface-3'}"
      >
        <span class={tab.icon}></span>
        {tab.label}
      </button>
    {/each}
  </div>

  <!-- Tab content -->
  <div class="flex-1 overflow-auto">
    {#if activeTab === 'servers'}
      <ServersSettings />
    {:else if activeTab === 'p2p'}
      <P2PSettings />
    {:else if activeTab === 'storage'}
      <StorageSettings />
    {:else if activeTab === 'app'}
      <AppSettings />
    {/if}
  </div>
</div>
