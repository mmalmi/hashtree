<script lang="ts">
  /**
   * SettingsPage - app settings
   * Port of React SettingsPanel
   */
  import { nip19 } from 'nostr-tools';
  import { nostrStore, type RelayStatus, getNsec } from '../nostr';
  import { appStore, formatBytes, formatBandwidth, updateStorageStats, refreshWebRTCStats, getLifetimeStats, blockPeer, unblockPeer } from '../store';
  import { socialGraphStore, getGraphSize, getFollows } from '../utils/socialGraph';
  import { settingsStore, DEFAULT_NETWORK_SETTINGS, DEFAULT_IMGPROXY_SETTINGS, DEFAULT_STORAGE_SETTINGS } from '../stores/settings';
  import { blossomLogStore } from '../stores/blossomLog';
  import { BackButton } from './ui';
  import { UserRow } from './User';
  import { isTauri, isAutostartEnabled, toggleAutostart } from '../tauri';
  import { isFilesApp } from '../appType';
  // Worker backend info
  let workerBackend = $derived(isTauri() ? 'Tauri (Rust)' : 'Web Worker');

  // Check if user is logged in with nsec (can copy secret key)
  let nsec = $derived(getNsec());
  let copiedNsec = $state(false);

  // Republish state
  let isRepublishing = $state(false);
  let republishResult = $state<number | null>(null);
  let blossomProgress = $state<{ treeName: string; current: number; total: number } | null>(null);
  let republishPrefix = $state('');
  let encryptionErrors = $state<string[]>([]);
  let isReencrypting = $state(false);
  let reencryptProgress = $state<{ current: number; total: number } | null>(null);

  async function copySecretKey() {
    const key = getNsec();
    if (!key) return;
    try {
      await navigator.clipboard.writeText(key);
      copiedNsec = true;
      setTimeout(() => (copiedNsec = false), 2000);
    } catch (e) {
      console.error('Failed to copy:', e);
    }
  }

  async function handleRepublish() {
    if (isRepublishing) return;
    isRepublishing = true;
    republishResult = null;
    blossomProgress = null;
    encryptionErrors = [];
    try {
      const { getWorkerAdapter } = await import('../workerAdapter');
      const adapter = getWorkerAdapter();
      if (adapter) {
        // Set up progress callbacks
        adapter.onBlossomPushProgress((treeName, current, total) => {
          blossomProgress = { treeName, current, total };
        });
        adapter.onBlossomPushComplete(() => {
          blossomProgress = null;
        });

        // URL-encode the prefix if provided
        const prefix = republishPrefix.trim() ? encodeURIComponent(republishPrefix.trim()) : undefined;
        const result = await adapter.republishTrees(prefix);
        republishResult = result.count;
        blossomProgress = null;

        // Check for encryption errors - auto fix them
        console.log('[Settings] Republish result:', result);
        if (result.encryptionErrors && result.encryptionErrors.length > 0) {
          console.log('[Settings] Found encryption errors, re-encrypting:', result.encryptionErrors);
          encryptionErrors = result.encryptionErrors;
          await handleReencrypt();
        } else {
          console.log('[Settings] No encryption errors detected');
          setTimeout(() => (republishResult = null), 5000);
        }
      }
    } catch (e) {
      console.error('Failed to republish:', e);
    } finally {
      isRepublishing = false;
      blossomProgress = null;
    }
  }

  async function handleReencrypt() {
    if (isReencrypting || encryptionErrors.length === 0 || !myPubkey) return;
    isReencrypting = true;
    reencryptProgress = { current: 0, total: encryptionErrors.length };

    try {
      const { reencryptSingleTree } = await import('../migrations/reencrypt');
      const npub = nip19.npubEncode(myPubkey);

      for (let i = 0; i < encryptionErrors.length; i++) {
        const treeName = encryptionErrors[i];
        reencryptProgress = { current: i + 1, total: encryptionErrors.length };

        try {
          // Get current tree root
          const { getTreeRootSync } = await import('../stores/treeRoot');
          const cid = getTreeRootSync(npub, treeName);
          if (!cid) {
            console.warn('[Settings] No CID for tree:', treeName);
            continue;
          }

          // Force re-encrypt - this also publishes to Nostr AND pushes to Blossom
          await reencryptSingleTree(npub, treeName, cid, 'public', true);
        } catch (e) {
          console.error('[Settings] Failed to re-encrypt:', treeName, e);
        }
      }

      encryptionErrors = [];
      setTimeout(() => (republishResult = null), 5000);
    } catch (e) {
      console.error('Failed to re-encrypt:', e);
    } finally {
      isReencrypting = false;
      reencryptProgress = null;
    }
  }

  let relayStatuses = $derived($nostrStore.relayStatuses);
  let discoveredRelays = $derived($nostrStore.discoveredRelays);
  let isLoggedIn = $derived($nostrStore.isLoggedIn);
  let myPubkey = $derived($nostrStore.pubkey);

  // Collapsible state for discovered relays
  let showDiscoveredRelays = $state(false);

  // Tauri desktop app settings
  let isDesktopApp = $state(false);
  let autostartEnabled = $state(false);
  let autostartLoading = $state(false);

  // Initialize Tauri state
  $effect(() => {
    isDesktopApp = isTauri();
    if (isDesktopApp) {
      isAutostartEnabled().then((enabled) => {
        autostartEnabled = enabled;
      });
    }
  });

  async function handleAutostartToggle() {
    if (autostartLoading) return;
    autostartLoading = true;
    const newValue = !autostartEnabled;
    const success = await toggleAutostart(newValue);
    if (success) {
      autostartEnabled = newValue;
    }
    autostartLoading = false;
  }

  // Network settings
  let networkSettings = $derived($settingsStore.network);
  let newRelayUrl = $state('');
  let newBlossomUrl = $state('');
  let editingRelays = $state(false);
  let editingBlossom = $state(false);
  let editingImgproxy = $state(false);

  // Imgproxy settings
  let imgproxySettings = $derived($settingsStore.imgproxy);

  // Pool settings
  let poolSettings = $derived($settingsStore.pools);

  // Storage settings
  let storageSettings = $derived($settingsStore.storage);
  let editingStorage = $state(false);

  // Blossom log
  let blossomLogs = $derived($blossomLogStore);

  function addRelay() {
    const url = newRelayUrl.trim();
    if (!url) return;
    // Validate URL
    try {
      new URL(url);
      if (!url.startsWith('wss://') && !url.startsWith('ws://')) {
        return;
      }
    } catch {
      return;
    }
    if (!networkSettings.relays.includes(url)) {
      settingsStore.setNetworkSettings({
        relays: [...networkSettings.relays, url],
      });
    }
    newRelayUrl = '';
  }

  function removeRelay(url: string) {
    settingsStore.setNetworkSettings({
      relays: networkSettings.relays.filter(r => r !== url),
    });
  }

  function resetRelays() {
    settingsStore.setNetworkSettings({
      relays: DEFAULT_NETWORK_SETTINGS.relays,
    });
    editingRelays = false;
  }

  function addBlossomServer() {
    const url = newBlossomUrl.trim();
    if (!url) return;
    // Validate URL
    try {
      new URL(url);
      if (!url.startsWith('https://') && !url.startsWith('http://')) {
        return;
      }
    } catch {
      return;
    }
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
    settingsStore.setNetworkSettings({
      blossomServers: DEFAULT_NETWORK_SETTINGS.blossomServers,
    });
    editingBlossom = false;
  }

  // Social graph stats - depend on version to trigger re-computation
  let isRecrawling = $derived($socialGraphStore.isRecrawling);
  let graphSize = $derived.by(() => {
    $socialGraphStore.version; // Track version changes
    return getGraphSize();
  });
  let myFollowsCount = $derived.by(() => {
    $socialGraphStore.version; // Track version changes
    return myPubkey ? getFollows(myPubkey).size : 0;
  });

  function getStatusColor(status: RelayStatus): string {
    switch (status) {
      case 'connected': return 'bg-success';
      case 'connecting': return 'bg-warning';
      case 'error': return 'bg-danger';
      default: return 'bg-text-3';
    }
  }

  function getRelayStatus(url: string): RelayStatus {
    // Normalize URL for lookup (remove trailing slash)
    const normalized = url.replace(/\/$/, '');
    return relayStatuses.get(normalized) || relayStatuses.get(url) || 'disconnected';
  }

  // App store - use $derived from the store directly
  let appState = $derived($appStore);
  let stats = $derived(appState.stats);

  // Use appStore.peers for the list (same source as indicator) - only show connected peers
  let peerList = $derived(appState.peers
    .filter(p => p.state === 'connected')
    .map(p => ({
      peerId: p.peerId,
      pubkey: p.pubkey,
      state: p.state,
      pool: p.pool,
      bytesSent: p.bytesSent,
      bytesReceived: p.bytesReceived,
    })));
  let myPeerId = $state<string | null>(null);
  let webrtcStats = $derived({
    bytesSent: peerList.reduce((sum, p) => sum + p.bytesSent, 0),
    bytesReceived: peerList.reduce((sum, p) => sum + p.bytesReceived, 0),
    bytesForwarded: 0,
  });
  let perPeerStats = $derived(new Map(
    peerList.map(p => [p.peerId, { bytesSent: p.bytesSent, bytesReceived: p.bytesReceived, bytesForwarded: 0 }])
  ));
  let uploadBandwidth = $state(0);
  let downloadBandwidth = $state(0);

  // Blocked peers from settings
  let blockedPeers = $derived($settingsStore.blockedPeers ?? []);

  // Lifetime stats (recalculated on each render when webrtcStats changes)
  let lifetimeStats = $derived.by(() => {
    // Trigger recalculation when webrtcStats changes
    webrtcStats;
    return getLifetimeStats();
  });

  // Load initial data on mount and refresh stats periodically
  $effect(() => {
    updateStorageStats();
    refreshWebRTCStats();

    // Refresh WebRTC stats every second while on settings page
    const statsInterval = setInterval(() => {
      refreshWebRTCStats();
    }, 1000);
    return () => clearInterval(statsInterval);
  });

  // Helper function to extract uuid from peerId (format: "pubkey:uuid")
  function getPeerUuid(peerId: string): string {
    return peerId.split(':')[1] || peerId;
  }

  // Helper function to get state color
  function stateColor(state: string): string {
    switch (state) {
      case 'connected': return '#3fb950';
      case 'connecting': return '#d29922';
      case 'failed': return '#f85149';
      default: return '#8b949e';
    }
  }

  // Helper function to get peer stats
  function getPeerStats(peerId: string) {
    return perPeerStats?.get(peerId);
  }
</script>

<div class="flex-1 flex flex-col min-h-0 bg-surface-0">
  <!-- Header -->
  <div class="border-b border-surface-3 shrink-0">
    <div class="h-12 px-4 flex items-center gap-3 w-full max-w-2xl mx-auto">
      <BackButton href="/" useHistory />
      <span class="font-semibold text-text-1">Settings</span>
    </div>
  </div>

  <!-- Content -->
  <div class="flex-1 overflow-y-auto p-4 space-y-6 w-full max-w-2xl mx-auto">
    <!-- Relays -->
    <div>
      <div class="flex items-center justify-between mb-1">
        <h3 class="text-xs font-medium text-muted uppercase tracking-wide">
          Relays ({networkSettings.relays.length})
        </h3>
        <button
          onclick={() => editingRelays = !editingRelays}
          class="btn-ghost text-xs"
        >
          {editingRelays ? 'Done' : 'Edit'}
        </button>
      </div>
      <p class="text-xs text-text-3 mb-3">Nostr servers used to find peers and npub/path directories</p>
      <div class="bg-surface-2 rounded divide-y divide-surface-3">
        {#each networkSettings.relays as relay (relay)}
          {@const status = getRelayStatus(relay)}
          <div class="flex items-center gap-2 p-3 text-sm">
            <span class="w-2 h-2 rounded-full {getStatusColor(status)} shrink-0"></span>
            <span class="text-text-1 truncate flex-1">
              {(() => {
                try {
                  return new URL(relay).hostname;
                } catch {
                  return relay;
                }
              })()}
            </span>
            {#if editingRelays}
              <button
                onclick={() => removeRelay(relay)}
                class="btn-ghost p-1 text-danger"
                title="Remove relay"
              >
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
        <button onclick={resetRelays} class="btn-ghost mt-2 text-xs text-text-3">
          Reset to defaults
        </button>
      {/if}

      <!-- Discovered Relays (collapsible) -->
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
                  {(() => {
                    try {
                      return new URL(relay.url).hostname;
                    } catch {
                      return relay.url;
                    }
                  })()}
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
        <button
          onclick={() => editingBlossom = !editingBlossom}
          class="btn-ghost text-xs"
        >
          {editingBlossom ? 'Done' : 'Edit'}
        </button>
      </div>
      <p class="text-xs text-text-3 mb-3">
        <a href="https://github.com/hzrd149/blossom" target="_blank" rel="noopener" class="text-accent hover:underline">Blossom</a> servers for fallback when peer-to-peer (WebRTC) connections are unavailable
      </p>
      <div class="bg-surface-2 rounded divide-y divide-surface-3">
        {#each networkSettings.blossomServers as server (server.url)}
          <div class="flex items-center gap-2 p-3 text-sm">
            <span class="i-lucide-server text-text-3 shrink-0"></span>
            <span class="text-text-1 truncate flex-1">
              {(() => {
                try {
                  return new URL(server.url).hostname;
                } catch {
                  return server.url;
                }
              })()}
            </span>
            <label class="flex items-center gap-1 text-xs text-text-3 cursor-pointer" title="Allow reads from this server">
              <input
                type="checkbox"
                checked={server.read}
                onchange={() => toggleBlossomRead(server.url)}
                class="accent-accent"
              />
              read
            </label>
            <label class="flex items-center gap-1 text-xs text-text-3 cursor-pointer" title="Allow uploads to this server">
              <input
                type="checkbox"
                checked={server.write}
                onchange={() => toggleBlossomWrite(server.url)}
                class="accent-accent"
              />
              write
            </label>
            {#if editingBlossom}
              <button
                onclick={() => removeBlossomServer(server.url)}
                class="btn-ghost p-1 text-danger"
                title="Remove server"
              >
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
        <button onclick={resetBlossomServers} class="btn-ghost mt-2 text-xs text-text-3">
          Reset to defaults
        </button>
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

    <!-- Image Proxy -->
    <div>
      <div class="flex items-center justify-between mb-1">
        <h3 class="text-xs font-medium text-muted uppercase tracking-wide">
          Image Proxy
        </h3>
        <button
          onclick={() => editingImgproxy = !editingImgproxy}
          class="btn-ghost text-xs"
        >
          {editingImgproxy ? 'Done' : 'Edit'}
        </button>
      </div>
      <p class="text-xs text-text-3 mb-3">Proxy external images for privacy and performance</p>
      <div class="bg-surface-2 rounded divide-y divide-surface-3">
        <!-- Enabled toggle -->
        <div class="flex items-center justify-between p-3">
          <span class="text-sm text-text-1">Enabled</span>
          <button
            onclick={() => settingsStore.setImgproxySettings({ enabled: !imgproxySettings.enabled })}
            class="relative w-11 h-6 rounded-full transition-colors {imgproxySettings.enabled ? 'bg-accent' : 'bg-surface-3'}"
            aria-checked={imgproxySettings.enabled}
            aria-label="Toggle image proxy"
            role="switch"
          >
            <span
              class="absolute top-1 left-1 w-4 h-4 bg-white rounded-full transition-transform shadow-sm {imgproxySettings.enabled ? 'translate-x-5' : 'translate-x-0'}"
            ></span>
          </button>
        </div>
        {#if editingImgproxy}
          <!-- URL -->
          <div class="p-3">
            <label class="flex flex-col gap-1">
              <span class="text-xs text-text-3">Proxy URL</span>
              <input
                type="url"
                value={imgproxySettings.url}
                oninput={(e) => settingsStore.setImgproxySettings({ url: e.currentTarget.value })}
                placeholder={DEFAULT_IMGPROXY_SETTINGS.url}
                class="input text-sm"
              />
            </label>
          </div>
          <!-- Key -->
          <div class="p-3">
            <label class="flex flex-col gap-1">
              <span class="text-xs text-text-3">Key (hex)</span>
              <input
                type="text"
                value={imgproxySettings.key}
                oninput={(e) => settingsStore.setImgproxySettings({ key: e.currentTarget.value })}
                placeholder={DEFAULT_IMGPROXY_SETTINGS.key.slice(0, 16) + '...'}
                class="input text-sm font-mono"
              />
            </label>
          </div>
          <!-- Salt -->
          <div class="p-3">
            <label class="flex flex-col gap-1">
              <span class="text-xs text-text-3">Salt (hex)</span>
              <input
                type="text"
                value={imgproxySettings.salt}
                oninput={(e) => settingsStore.setImgproxySettings({ salt: e.currentTarget.value })}
                placeholder={DEFAULT_IMGPROXY_SETTINGS.salt.slice(0, 16) + '...'}
                class="input text-sm font-mono"
              />
            </label>
          </div>
        {:else}
          <!-- Show current settings when not editing -->
          <div class="p-3 text-sm">
            <span class="text-text-3">URL: </span>
            <span class="text-text-1">{imgproxySettings.url}</span>
          </div>
          <div class="p-3 text-sm">
            <span class="text-text-3">Key: </span>
            <span class="text-text-1 font-mono text-xs">{imgproxySettings.key.slice(0, 16)}...</span>
          </div>
          <div class="p-3 text-sm">
            <span class="text-text-3">Salt: </span>
            <span class="text-text-1 font-mono text-xs">{imgproxySettings.salt.slice(0, 16)}...</span>
          </div>
        {/if}
      </div>
      {#if editingImgproxy}
        <button onclick={() => { settingsStore.resetImgproxySettings(); editingImgproxy = false; }} class="btn-ghost mt-2 text-xs text-text-3">
          Reset to defaults
        </button>
      {/if}
    </div>

    <!-- WebRTC Pool Settings -->
    <div>
      <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-1">
        WebRTC Pools
      </h3>
      <p class="text-xs text-text-3 mb-3">Max peer connections by category</p>
      <div class="bg-surface-2 rounded divide-y divide-surface-3">
        <!-- Follows pool -->
        <div class="p-3">
          <div class="flex items-center justify-between mb-2">
            <span class="text-sm text-text-1">Follows</span>
            <span class="text-xs text-text-3">Peers you follow</span>
          </div>
          <div class="grid grid-cols-2 gap-3 text-sm">
            <label class="flex flex-col gap-1">
              <span class="text-xs text-text-3">Max</span>
              <input
                type="number"
                min="1"
                max="100"
                value={poolSettings.followsMax}
                onchange={(e) => settingsStore.setPoolSettings({ followsMax: parseInt(e.currentTarget.value) || 20 })}
                class="input text-sm"
              />
            </label>
            <label class="flex flex-col gap-1">
              <span class="text-xs text-text-3">Satisfied</span>
              <input
                type="number"
                min="1"
                max="100"
                value={poolSettings.followsSatisfied}
                onchange={(e) => settingsStore.setPoolSettings({ followsSatisfied: parseInt(e.currentTarget.value) || 10 })}
                class="input text-sm"
              />
            </label>
          </div>
        </div>
        <!-- Others pool -->
        <div class="p-3">
          <div class="flex items-center justify-between mb-2">
            <span class="text-sm text-text-1">Others</span>
            <span class="text-xs text-text-3">Other peers</span>
          </div>
          <div class="grid grid-cols-2 gap-3 text-sm">
            <label class="flex flex-col gap-1">
              <span class="text-xs text-text-3">Max</span>
              <input
                type="number"
                min="0"
                max="100"
                value={poolSettings.otherMax}
                onchange={(e) => { const v = parseInt(e.currentTarget.value); settingsStore.setPoolSettings({ otherMax: isNaN(v) ? 10 : v }); }}
                class="input text-sm"
              />
            </label>
            <label class="flex flex-col gap-1">
              <span class="text-xs text-text-3">Satisfied</span>
              <input
                type="number"
                min="0"
                max="100"
                value={poolSettings.otherSatisfied}
                onchange={(e) => { const v = parseInt(e.currentTarget.value); settingsStore.setPoolSettings({ otherSatisfied: isNaN(v) ? 5 : v }); }}
                class="input text-sm"
              />
            </label>
          </div>
        </div>
      </div>
      <button onclick={() => settingsStore.resetPoolSettings()} class="btn-ghost mt-2 text-xs text-text-3">
        Reset to defaults
      </button>

      <!-- Header Display Settings -->
      <div class="bg-surface-2 rounded divide-y divide-surface-3 mt-3">
        <label class="p-3 flex items-center justify-between cursor-pointer">
          <div>
            <span class="text-sm text-text-1">Show connectivity</span>
            <p class="text-xs text-text-3">Display connection status in header</p>
          </div>
          <input
            type="checkbox"
            checked={poolSettings.showConnectivity ?? true}
            onchange={(e) => settingsStore.setPoolSettings({ showConnectivity: e.currentTarget.checked })}
            class="w-4 h-4 accent-accent"
          />
        </label>
        <label class="p-3 flex items-center justify-between cursor-pointer">
          <div>
            <span class="text-sm text-text-1">Show bandwidth</span>
            <p class="text-xs text-text-3">Display upload/download rates in header</p>
          </div>
          <input
            type="checkbox"
            checked={poolSettings.showBandwidth ?? false}
            onchange={(e) => settingsStore.setPoolSettings({ showBandwidth: e.currentTarget.checked })}
            class="w-4 h-4 accent-accent"
          />
        </label>
      </div>
    </div>

    <!-- Peers -->
    <div>
      <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-1">
        Peers ({peerList.length})
      </h3>
      <p class="text-xs text-text-3 mb-3">WebRTC connections for file exchange</p>

      <!-- Transfer stats -->
      {#if isLoggedIn}
        <div class="bg-surface-2 rounded p-3 mb-3">
          <!-- Bandwidth -->
          <div class="grid grid-cols-2 gap-x-4 gap-y-2 text-xs mb-3 pb-3 border-b border-surface-3">
            <div class="flex justify-between">
              <span class="text-text-3">
                <span class="i-lucide-arrow-up inline-block align-middle mr-1 text-success"></span>Upload
              </span>
              <span class="text-text-1 font-mono">{formatBandwidth(uploadBandwidth)}</span>
            </div>
            <div class="flex justify-between">
              <span class="text-text-3">
                <span class="i-lucide-arrow-down inline-block align-middle mr-1 text-accent"></span>Download
              </span>
              <span class="text-text-1 font-mono">{formatBandwidth(downloadBandwidth)}</span>
            </div>
          </div>
          <!-- Session transfer -->
          <div class="grid grid-cols-3 gap-x-3 gap-y-2 text-xs mb-3">
            <div class="text-text-3 text-center">Session</div>
            <div class="text-center">
              <span class="text-success font-mono">{formatBytes(webrtcStats?.bytesSent ?? 0)}</span>
              <span class="text-text-3 ml-1">up</span>
            </div>
            <div class="text-center">
              <span class="text-accent font-mono">{formatBytes(webrtcStats?.bytesReceived ?? 0)}</span>
              <span class="text-text-3 ml-1">down</span>
            </div>
          </div>
          <!-- Lifetime transfer -->
          <div class="grid grid-cols-3 gap-x-3 gap-y-2 text-xs">
            <div class="text-text-3 text-center">Lifetime</div>
            <div class="text-center">
              <span class="text-success font-mono">{formatBytes(lifetimeStats.bytesSent)}</span>
              <span class="text-text-3 ml-1">up</span>
            </div>
            <div class="text-center">
              <span class="text-accent font-mono">{formatBytes(lifetimeStats.bytesReceived)}</span>
              <span class="text-text-3 ml-1">down</span>
            </div>
          </div>
          {#if lifetimeStats.bytesForwarded > 0}
            <div class="text-xs text-text-3 mt-2 text-center">
              Forwarded: {formatBytes(lifetimeStats.bytesForwarded)}
            </div>
          {/if}
        </div>
      {/if}

      {#if myPeerId}
        <div class="text-xs text-muted mb-2 font-mono">
          Your ID: {myPeerId}
        </div>
      {/if}
      {#if !isLoggedIn}
        <div class="bg-surface-2 rounded p-3 text-sm text-muted">
          Login to connect with peers
        </div>
      {:else if peerList.length === 0}
        <div class="bg-surface-2 rounded p-3 text-sm text-muted">
          No peers connected
        </div>
      {:else}
        <div class="bg-surface-2 rounded divide-y divide-surface-3">
          {#each peerList as peer (peer.peerId)}
            {@const peerStats = getPeerStats(peer.peerId)}
            <div class="flex flex-col p-3 hover:bg-surface-3 transition-colors">
              <div class="flex items-center gap-2 text-sm">
                <span
                  class="w-2 h-2 rounded-full shrink-0"
                  style="background: {stateColor(peer.state)}"
                ></span>
                <a
                  href="#/{nip19.npubEncode(peer.pubkey)}"
                  class="flex-1 min-w-0 no-underline"
                >
                  <UserRow
                    pubkey={peer.pubkey}
                    description={peer.isSelf ? 'You' : `${peer.state}${peer.pool === 'follows' ? ' (follow)' : ''}`}
                    avatarSize={32}
                    showBadge
                    class="flex-1 min-w-0"
                  />
                </a>
                <span class="text-xs text-muted font-mono shrink-0">
                  {getPeerUuid(peer.peerId).slice(0, 8)}
                </span>
                {#if !peer.isSelf}
                  <button
                    onclick={(e) => {
                      e.preventDefault();
                      e.stopPropagation();
                      if (confirm('Block this peer? They will be disconnected and won\'t be able to connect again.')) {
                        blockPeer(peer.pubkey);
                      }
                    }}
                    class="btn-ghost p-1 text-text-3 hover:text-danger shrink-0"
                    title="Block peer"
                  >
                    <span class="i-lucide-ban text-sm"></span>
                  </button>
                {/if}
              </div>
              <!-- Per-peer stats -->
              {#if peerStats && peer.state === 'connected'}
                <div class="mt-2 ml-4 flex flex-wrap gap-x-3 gap-y-1 text-xs text-text-3">
                  <span title="Bytes sent to this peer" class="text-success">
                    <span class="i-lucide-arrow-up inline-block align-middle mr-0.5"></span>{formatBytes(peerStats.bytesSent)}
                  </span>
                  <span title="Bytes received from this peer" class="text-accent">
                    <span class="i-lucide-arrow-down inline-block align-middle mr-0.5"></span>{formatBytes(peerStats.bytesReceived)}
                  </span>
                  {#if peerStats.bytesForwarded > 0}
                    <span title="Bytes forwarded for this peer">
                      <span class="i-lucide-forward inline-block align-middle mr-0.5"></span>{formatBytes(peerStats.bytesForwarded)}
                    </span>
                  {/if}
                  {#if peerStats.receiveErrors > 0}
                    <span title="Receive errors from this peer" class="text-danger">
                      <span class="i-lucide-alert-triangle inline-block align-middle mr-0.5"></span>{peerStats.receiveErrors}
                    </span>
                  {/if}
                </div>
              {/if}
            </div>
          {/each}
        </div>
      {/if}

      <!-- Blocked Peers -->
      {#if blockedPeers.length > 0}
        <div class="mt-4">
          <h4 class="text-xs font-medium text-muted uppercase tracking-wide mb-2">
            Blocked ({blockedPeers.length})
          </h4>
          <div class="bg-surface-2 rounded divide-y divide-surface-3">
            {#each blockedPeers as pubkey (pubkey)}
              <div class="flex items-center gap-2 p-2">
                <a
                  href="#/{nip19.npubEncode(pubkey)}"
                  class="flex-1 min-w-0 no-underline"
                >
                  <UserRow
                    {pubkey}
                    description="Blocked"
                    avatarSize={28}
                    showBadge
                    class="flex-1 min-w-0"
                  />
                </a>
                <button
                  onclick={() => unblockPeer(pubkey)}
                  class="btn-ghost p-1 text-text-3 hover:text-success shrink-0"
                  title="Unblock peer"
                >
                  <span class="i-lucide-check text-sm"></span>
                </button>
              </div>
            {/each}
          </div>
        </div>
      {/if}
    </div>

    <!-- Social Graph -->
    <div>
      <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-1 flex items-center gap-2">
        Social Graph
        {#if isRecrawling}
          <span class="text-xs text-accent animate-pulse">crawling...</span>
        {/if}
      </h3>
      <p class="text-xs text-text-3 mb-3">Follow network used for trust indicators</p>
      <div class="bg-surface-2 rounded p-3 text-sm space-y-2">
        <div class="flex justify-between">
          <span class="text-muted">Users in graph</span>
          <span class="text-text-1">{graphSize.toLocaleString()}</span>
        </div>
        <div class="flex justify-between">
          <span class="text-muted">Following</span>
          <span class="text-text-1">{myFollowsCount.toLocaleString()}</span>
        </div>
      </div>
    </div>

    <!-- Worker Backend -->
    <div>
      <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-3">
        Worker Backend
      </h3>
      <div class="bg-surface-2 rounded p-3 text-sm">
        <div class="flex justify-between">
          <span class="text-muted">Type</span>
          <span class="text-text-1 flex items-center gap-1.5">
            {#if isTauri()}
              <span class="w-2 h-2 rounded-full bg-success"></span>
            {:else}
              <span class="w-2 h-2 rounded-full bg-info"></span>
            {/if}
            {workerBackend}
          </span>
        </div>
      </div>
    </div>

    <!-- Local Storage -->
    <div>
      <div class="flex items-center justify-between mb-1">
        <h3 class="text-xs font-medium text-muted uppercase tracking-wide">
          Local Storage
        </h3>
        <button
          onclick={() => editingStorage = !editingStorage}
          class="btn-ghost text-xs"
        >
          {editingStorage ? 'Done' : 'Edit'}
        </button>
      </div>
      <p class="text-xs text-text-3 mb-3">IndexedDB cache for downloaded content</p>
      <div class="bg-surface-2 rounded p-3 text-sm space-y-2">
        <div class="flex justify-between">
          <span class="text-muted">Items</span>
          <span class="text-text-1" data-testid="storage-items">{stats.items.toLocaleString()}</span>
        </div>
        <div class="flex justify-between">
          <span class="text-muted">Size</span>
          <span class="text-text-1" data-testid="storage-size">{formatBytes(stats.bytes)}</span>
        </div>
        <div class="flex justify-between">
          <span class="text-muted">Limit</span>
          <span class="text-text-1" data-testid="storage-limit">{formatBytes(storageSettings.maxBytes)}</span>
        </div>
        {#if stats.bytes > 0}
          <div class="flex justify-between">
            <span class="text-muted">Usage</span>
            <span class="text-text-1">{Math.round((stats.bytes / storageSettings.maxBytes) * 100)}%</span>
          </div>
        {/if}
      </div>
      {#if editingStorage}
        <div class="mt-2 bg-surface-2 rounded p-3">
          <label class="flex flex-col gap-1">
            <span class="text-xs text-text-3">Storage limit (MB)</span>
            <input
              type="number"
              min="100"
              max="10000"
              step="100"
              value={Math.round(storageSettings.maxBytes / 1024 / 1024)}
              onchange={(e) => {
                const mb = parseInt(e.currentTarget.value) || 1024;
                settingsStore.setStorageSettings({ maxBytes: mb * 1024 * 1024 });
              }}
              class="input text-sm"
              data-testid="storage-limit-input"
            />
          </label>
          <p class="text-xs text-text-3 mt-2">Oldest data will be evicted when limit is exceeded</p>
        </div>
        <button onclick={() => { settingsStore.resetStorageSettings(); editingStorage = false; }} class="btn-ghost mt-2 text-xs text-text-3">
          Reset to default ({Math.round(DEFAULT_STORAGE_SETTINGS.maxBytes / 1024 / 1024)} MB)
        </button>
      {/if}
    </div>

    <!-- Data Sync (only show when logged in) -->
    {#if isLoggedIn}
      <div>
        <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-1">
          Data Sync
        </h3>
        <p class="text-xs text-text-3 mb-3">Republish your directories if they're not syncing to other devices</p>
        <div class="bg-surface-2 rounded p-3 space-y-3">
          <div>
            <label for="republish-prefix" class="text-xs text-text-3 mb-1 block">Prefix filter (optional)</label>
            <input
              id="republish-prefix"
              type="text"
              bind:value={republishPrefix}
              placeholder="e.g. photos/ or projects/"
              class="input text-sm w-full"
              disabled={isRepublishing}
            />
          </div>
          <button
            onclick={handleRepublish}
            disabled={isRepublishing || isReencrypting}
            class="btn-ghost flex items-center gap-2 text-sm w-full justify-start"
          >
            {#if isReencrypting}
              <span class="i-lucide-loader-2 animate-spin"></span>
              <span>Re-encrypting {reencryptProgress?.current}/{reencryptProgress?.total}...</span>
            {:else if isRepublishing}
              <span class="i-lucide-loader-2 animate-spin"></span>
              <span>Republishing...</span>
            {:else if republishResult !== null}
              <span class="i-lucide-check text-success"></span>
              <span>Republished {republishResult} directories</span>
            {:else}
              <span class="i-lucide-upload-cloud"></span>
              <span>Republish {republishPrefix.trim() ? `"${republishPrefix.trim()}*"` : 'all'} directories to relays</span>
            {/if}
          </button>
          {#if blossomProgress}
            <div class="mt-2 flex items-center gap-2 text-xs text-text-3">
              <span class="i-lucide-loader-2 animate-spin text-accent"></span>
              <span>Uploading {blossomProgress.treeName}: {blossomProgress.current} chunks...</span>
            </div>
          {/if}
        </div>
      </div>
    {/if}

    <!-- Desktop App Settings (only show in Tauri) -->
    {#if isDesktopApp}
      <div>
        <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-3">
          Desktop App
        </h3>
        <div class="bg-surface-2 rounded divide-y divide-surface-3">
          <div class="flex items-center justify-between p-3">
            <div class="flex flex-col gap-1">
              <span class="text-sm text-text-1">Start on login</span>
              <span class="text-xs text-text-3">Launch Iris Files when you log in</span>
            </div>
            <button
              onclick={handleAutostartToggle}
              disabled={autostartLoading}
              class="relative w-11 h-6 rounded-full transition-colors {autostartEnabled ? 'bg-accent' : 'bg-surface-3'}"
              aria-checked={autostartEnabled}
              aria-label="Toggle start on login"
              role="switch"
            >
              <span
                class="absolute top-1 left-1 w-4 h-4 bg-white rounded-full transition-transform shadow-sm {autostartEnabled ? 'translate-x-5' : 'translate-x-0'}"
              ></span>
            </button>
          </div>
        </div>
      </div>
    {/if}

    <!-- Account (only show when logged in with nsec) -->
    {#if nsec}
      <div>
        <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-3">
          Account
        </h3>
        <div class="bg-surface-2 rounded p-3">
          <button
            onclick={copySecretKey}
            class="btn-ghost flex items-center gap-2 text-sm w-full justify-start"
            data-testid="copy-secret-key"
          >
            {#if copiedNsec}
              <span class="i-lucide-check text-success"></span>
              <span>Copied!</span>
            {:else}
              <span class="i-lucide-key"></span>
              <span>Copy secret key</span>
            {/if}
          </button>
        </div>
      </div>
    {/if}

    <!-- About -->
    <div>
      <h3 class="text-xs font-medium text-muted uppercase tracking-wide mb-3">
        About
      </h3>
      <p class="text-sm text-text-2 mb-3">
        hashtree - Content-addressed filesystem on Nostr
      </p>
      <div class="bg-surface-2 rounded p-3 text-sm space-y-3">
        <div class="flex justify-between items-center">
          <span class="text-muted">Build</span>
          <span class="text-text-1 font-mono text-xs">
            {(() => {
              const buildTime = import.meta.env.VITE_BUILD_TIME;
              if (!buildTime || buildTime === 'undefined') return 'development';
              try {
                return new Date(buildTime).toLocaleString();
              } catch {
                return buildTime;
              }
            })()}
          </span>
        </div>
        <a
          href={isFilesApp() ? "#/npub1xndmdgymsf4a34rzr7346vp8qcptxf75pjqweh8naa8rklgxpfqqmfjtce/hashtree-ts" : "https://files.iris.to/#/npub1xndmdgymsf4a34rzr7346vp8qcptxf75pjqweh8naa8rklgxpfqqmfjtce/hashtree-ts"}
          target={isFilesApp() ? undefined : "_blank"}
          rel={isFilesApp() ? undefined : "noopener noreferrer"}
          class="btn-ghost w-full flex items-center justify-center gap-2 no-underline"
        >
          <span class="i-lucide-code text-sm"></span>
          <span>hashtree-ts</span>
          <span class="text-text-3 text-xs no-underline">(TypeScript library & this app)</span>
        </a>
        <a
          href={isFilesApp() ? "#/npub1xndmdgymsf4a34rzr7346vp8qcptxf75pjqweh8naa8rklgxpfqqmfjtce/hashtree-rs" : "https://files.iris.to/#/npub1xndmdgymsf4a34rzr7346vp8qcptxf75pjqweh8naa8rklgxpfqqmfjtce/hashtree-rs"}
          target={isFilesApp() ? undefined : "_blank"}
          rel={isFilesApp() ? undefined : "noopener noreferrer"}
          class="btn-ghost w-full flex items-center justify-center gap-2 no-underline"
        >
          <span class="i-lucide-terminal text-sm"></span>
          <span>hashtree-rs</span>
          <span class="text-text-3 text-xs no-underline">(Rust CLI & P2P daemon)</span>
        </a>
        <a
          href={isFilesApp() ? "#/npub1xndmdgymsf4a34rzr7346vp8qcptxf75pjqweh8naa8rklgxpfqqmfjtce/hashtree-ci" : "https://files.iris.to/#/npub1xndmdgymsf4a34rzr7346vp8qcptxf75pjqweh8naa8rklgxpfqqmfjtce/hashtree-ci"}
          target={isFilesApp() ? undefined : "_blank"}
          rel={isFilesApp() ? undefined : "noopener noreferrer"}
          class="btn-ghost w-full flex items-center justify-center gap-2 no-underline"
        >
          <span class="i-lucide-bot text-sm"></span>
          <span>hashtree-ci</span>
          <span class="text-text-3 text-xs no-underline">(CI/CD pipeline)</span>
        </a>
        <button
          onclick={() => window.location.reload()}
          class="btn-ghost w-full flex items-center justify-center gap-2"
        >
          <span class="i-lucide-refresh-cw text-sm"></span>
          <span>Refresh App</span>
        </button>
      </div>
    </div>
  </div>
</div>
