<script lang="ts">
  /**
   * Simplified connectivity indicator - network icon with count
   * Red: not connected, Yellow: relays only, Green: peers, Blue: follows peers
   * Shows "offline" text when browser is offline
   * Clicking navigates to settings
   */
  import { appStore } from '../store';
  import { nostrStore } from '../nostr';

  let peerCount = $derived($appStore.peerCount);
  let peersList = $derived($appStore.peers);
  let connectedRelays = $derived($nostrStore.connectedRelays);
  let configuredRelays = $derived($nostrStore.relays);
  let configuredRelayCount = $derived(configuredRelays.length);

  let displayRelays = $derived.by(() => connectedRelays > 0 ? connectedRelays : configuredRelayCount);

  // Count peers in follows pool
  let followsPeers = $derived(peersList.filter(p => p.pool === 'follows' && p.state === 'connected').length);

  // Track browser online/offline status
  let isOnline = $state(typeof navigator !== 'undefined' ? navigator.onLine : true);

  $effect(() => {
    const handleOnline = () => { isOnline = true; };
    const handleOffline = () => { isOnline = false; };

    window.addEventListener('online', handleOnline);
    window.addEventListener('offline', handleOffline);

    return () => {
      window.removeEventListener('online', handleOnline);
      window.removeEventListener('offline', handleOffline);
    };
  });

  // Color logic: red = offline/none, yellow = relays only/connecting, green = peers, blue = follows peers
  let color = $derived.by(() => {
    if (!isOnline) return '#f85149'; // red when offline
    if (connectedRelays === 0) return configuredRelayCount > 0 ? '#d29922' : '#f85149'; // yellow if connecting
    if (peerCount === 0) return '#d29922'; // yellow
    if (followsPeers > 0) return '#58a6ff'; // blue - connected to follows
    return '#3fb950'; // green - connected to other peers
  });

  let title = $derived.by(() => {
    if (!isOnline) return 'Offline';
    if (connectedRelays === 0) {
      return configuredRelayCount > 0
        ? `Connecting to ${configuredRelayCount} relay${configuredRelayCount !== 1 ? 's' : ''}`
        : 'No relays configured';
    }
    if (peerCount === 0) return `${connectedRelays} relay${connectedRelays !== 1 ? 's' : ''}, no peers`;
    if (followsPeers > 0) {
      return `${followsPeers} follow${followsPeers !== 1 ? 's' : ''}, ${peerCount - followsPeers} other, ${connectedRelays} relay${connectedRelays !== 1 ? 's' : ''}`;
    }
    return `${peerCount} peer${peerCount !== 1 ? 's' : ''}, ${connectedRelays} relay${connectedRelays !== 1 ? 's' : ''}`;
  });

  // Total connections = relays + peers (show configured relays while connecting)
  let totalConnections = $derived(displayRelays + peerCount);
</script>

<a
  href="#/settings"
  class="flex flex-col items-center px-2 py-1 text-sm no-underline"
  data-testid="connectivity-indicator"
  {title}
>
  <div class="flex flex-col items-center">
    <span
      data-testid="peer-indicator-dot"
      class="i-lucide-wifi"
      style="color: {color}"
    ></span>
    <span data-testid="peer-count" class="text-xs -mt-1" style="color: {color}">{totalConnections}</span>
  </div>
  {#if !isOnline}
    <span class="text-[10px] text-danger -mt-0.5">offline</span>
  {/if}
</a>
