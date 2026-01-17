<script lang="ts">
  import { nostrStore, type RelayStatus } from '../../nostr';
  import { settingsStore, DEFAULT_NETWORK_SETTINGS } from '../../stores/settings';
  import { blossomLogStore } from '../../stores/blossomLog';

  // Network settings
  let networkSettings = $derived($settingsStore.network);
  let newRelayUrl = $state('');
  let newBlossomUrl = $state('');
  let editingRelays = $state(false);
  let editingBlossom = $state(false);

  // Relay statuses
  let relayStatuses = $derived($nostrStore.relayStatuses);
  let discoveredRelays = $derived(
    Array.from($nostrStore.discoveredRelays?.entries() || []).map(([url, status]) => ({ url, status }))
  );
  let showDiscoveredRelays = $state(false);

  // Blossom log
  let blossomLogs = $derived($blossomLogStore);

  function getStatusColor(status: RelayStatus): string {
    switch (status) {
      case 'connected': return 'bg-success';
      case 'connecting': return 'bg-warning';
      case 'error': return 'bg-danger';
      default: return 'bg-text-3';
    }
  }

  function getRelayStatus(url: string): RelayStatus {
    const normalized = url.replace(/\/$/, '');
    return relayStatuses.get(normalized) || relayStatuses.get(url) || 'disconnected';
  }

  function addRelay() {
    const url = newRelayUrl.trim();
    if (!url) return;
    try {
      new URL(url);
      if (!url.startsWith('wss://') && !url.startsWith('ws://')) return;
    } catch { return; }
    if (!networkSettings.relays.includes(url)) {
      settingsStore.setNetworkSettings({ relays: [...networkSettings.relays, url] });
    }
    newRelayUrl = '';
  }

  function removeRelay(url: string) {
    settingsStore.setNetworkSettings({ relays: networkSettings.relays.filter(r => r !== url) });
  }

  function resetRelays() {
    settingsStore.setNetworkSettings({ relays: DEFAULT_NETWORK_SETTINGS.relays });
    editingRelays = false;
  }

  function addBlossomServer() {
    const url = newBlossomUrl.trim();
    if (!url) return;
    try {
      new URL(url);
      if (!url.startsWith('https://') && !url.startsWith('http://')) return;
    } catch { return; }
    if (!networkSettings.blossomServers.some(s => s.url === url)) {
      settingsStore.setNetworkSettings({
        blossomServers: [...networkSettings.blossomServers, { url, read: true, write: false }],
      });
    }
    newBlossomUrl = '';
  }

  function removeBlossomServer(url: string) {
    settingsStore.setNetworkSettings({
      blossomServers: networkSettings.blossomServers.filter(s => s.url !== url),
    });
  }

  function toggleBlossomRead(url: string) {
    settingsStore.setNetworkSettings({
      blossomServers: networkSettings.blossomServers.map(s =>
        s.url === url ? { ...s, read: !s.read } : s
      ),
    });
  }

  function toggleBlossomWrite(url: string) {
    settingsStore.setNetworkSettings({
      blossomServers: networkSettings.blossomServers.map(s =>
        s.url === url ? { ...s, write: !s.write } : s
      ),
    });
  }

  function resetBlossomServers() {
    settingsStore.setNetworkSettings({ blossomServers: DEFAULT_NETWORK_SETTINGS.blossomServers });
    editingBlossom = false;
  }
</script>

<div class="p-4 space-y-6 max-w-2xl mx-auto">
  <!-- Relays -->
  <div>
    <div class="flex items-center justify-between mb-1">
      <h3 class="text-xs font-medium text-muted uppercase tracking-wide">
        Relays ({networkSettings.relays.length})
      </h3>
      <button onclick={() => editingRelays = !editingRelays} class="btn-ghost text-xs">
        {editingRelays ? 'Done' : 'Edit'}
      </button>
    </div>
    <p class="text-xs text-text-3 mb-3">Nostr servers for peer discovery and data sync</p>
    <div class="bg-surface-2 rounded divide-y divide-surface-3">
      {#each networkSettings.relays as relay (relay)}
        {@const status = getRelayStatus(relay)}
        <div class="flex items-center gap-2 p-3 text-sm">
          <span class="w-2 h-2 rounded-full {getStatusColor(status)} shrink-0"></span>
          <span class="text-text-1 truncate flex-1">
            {(() => { try { return new URL(relay).hostname; } catch { return relay; } })()}
          </span>
          {#if editingRelays}
            <button onclick={() => removeRelay(relay)} class="btn-ghost p-1 text-danger" title="Remove relay">
              <span class="i-lucide-x text-sm"></span>
            </button>
          {:else}
            <span class="text-xs text-text-3">{status.charAt(0).toUpperCase() + status.slice(1)}</span>
          {/if}
        </div>
      {/each}
    </div>
    {#if editingRelays}
      <div class="mt-2 flex gap-2">
        <input
          type="text"
          bind:value={newRelayUrl}
          placeholder="wss://relay.example.com"
          class="flex-1 input text-sm"
          onkeydown={(e) => e.key === 'Enter' && addRelay()}
        />
        <button onclick={addRelay} class="btn-primary text-sm">Add</button>
      </div>
      <button onclick={resetRelays} class="btn-ghost mt-2 text-xs text-text-3">Reset to defaults</button>
    {/if}

    <!-- Discovered Relays -->
    {#if discoveredRelays.length > 0}
      <div class="bg-surface-2 rounded mt-1">
        <button
          onclick={() => showDiscoveredRelays = !showDiscoveredRelays}
          class="btn-ghost b-0 flex items-center gap-1 p-3 text-sm text-text-1 w-full"
        >
          <span class="i-lucide-chevron-right text-sm transition-transform {showDiscoveredRelays ? 'rotate-90' : ''}"></span>
          Discovered relays ({discoveredRelays.length})
        </button>
        {#if showDiscoveredRelays}
          <div class="divide-y divide-surface-3">
            {#each discoveredRelays as relay (relay.url)}
              <div class="flex items-center gap-2 p-3 text-sm">
                <span class="w-2 h-2 rounded-full {getStatusColor(relay.status)} shrink-0"></span>
                <span class="text-text-1 truncate flex-1">
                  {(() => { try { return new URL(relay.url).hostname; } catch { return relay.url; } })()}
                </span>
                <span class="text-xs text-text-3">{relay.status.charAt(0).toUpperCase() + relay.status.slice(1)}</span>
              </div>
            {/each}
          </div>
        {/if}
      </div>
    {/if}
  </div>

  <!-- File Servers -->
  <div>
    <div class="flex items-center justify-between mb-1">
      <h3 class="text-xs font-medium text-muted uppercase tracking-wide">
        File Servers ({networkSettings.blossomServers.length})
      </h3>
      <button onclick={() => editingBlossom = !editingBlossom} class="btn-ghost text-xs">
        {editingBlossom ? 'Done' : 'Edit'}
      </button>
    </div>
    <p class="text-xs text-text-3 mb-3">
      <a href="https://github.com/hzrd149/blossom" target="_blank" rel="noopener" class="text-accent hover:underline">Blossom</a> servers for file storage fallback
    </p>
    <div class="bg-surface-2 rounded divide-y divide-surface-3">
      {#each networkSettings.blossomServers as server (server.url)}
        <div class="flex items-center gap-2 p-3 text-sm">
          <span class="i-lucide-server text-text-3 shrink-0"></span>
          <span class="text-text-1 truncate flex-1">
            {(() => { try { return new URL(server.url).hostname; } catch { return server.url; } })()}
          </span>
          <label class="flex items-center gap-1 text-xs text-text-3 cursor-pointer">
            <input type="checkbox" checked={server.read} onchange={() => toggleBlossomRead(server.url)} class="accent-accent" />
            read
          </label>
          <label class="flex items-center gap-1 text-xs text-text-3 cursor-pointer">
            <input type="checkbox" checked={server.write} onchange={() => toggleBlossomWrite(server.url)} class="accent-accent" />
            write
          </label>
          {#if editingBlossom}
            <button onclick={() => removeBlossomServer(server.url)} class="btn-ghost p-1 text-danger" title="Remove server">
              <span class="i-lucide-x text-sm"></span>
            </button>
          {/if}
        </div>
      {:else}
        <div class="p-3 text-sm text-text-3">No servers configured</div>
      {/each}
    </div>
    {#if editingBlossom}
      <div class="mt-2 flex gap-2">
        <input
          type="text"
          bind:value={newBlossomUrl}
          placeholder="https://blossom.example.com"
          class="flex-1 input text-sm"
          onkeydown={(e) => e.key === 'Enter' && addBlossomServer()}
        />
        <button onclick={addBlossomServer} class="btn-primary text-sm">Add</button>
      </div>
      <button onclick={resetBlossomServers} class="btn-ghost mt-2 text-xs text-text-3">Reset to defaults</button>
    {/if}

    <!-- Blossom Log -->
    {#if blossomLogs.length > 0}
      <div class="mt-3">
        <div class="flex items-center justify-between mb-1">
          <span class="text-xs text-text-3">Recent activity</span>
          <button onclick={() => blossomLogStore.clear()} class="btn-ghost text-xs text-text-3">Clear</button>
        </div>
        <div class="bg-surface-3 rounded text-xs font-mono max-h-32 overflow-y-auto overflow-x-auto p-2 space-y-1">
          {#each blossomLogs as log (log.timestamp + log.hash)}
            {@const time = new Date(log.timestamp).toLocaleTimeString()}
            <div class="flex items-center gap-2 whitespace-nowrap {log.success ? 'text-success' : 'text-danger'}">
              <span class="text-text-3">{time}</span>
              <span>{log.operation.toUpperCase()}</span>
              <span class="text-text-2">{log.server}</span>
              <span class="text-text-3">{log.hash.slice(0, 8)}...</span>
              {#if log.success && log.bytes}
                <span class="text-text-3">{log.bytes}B</span>
              {:else if log.error}
                <span>{log.error}</span>
              {/if}
            </div>
          {/each}
        </div>
      </div>
    {/if}
  </div>
</div>
