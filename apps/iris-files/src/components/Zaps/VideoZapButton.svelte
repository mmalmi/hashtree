<script lang="ts">
  /**
   * VideoZapButton - Shows zap total and allows zapping a video
   * Self-contained component with its own subscription
   */
  import { untrack } from 'svelte';
  import { nip19 } from 'nostr-tools';
  import { subscribeToZaps, insertZapSorted, type Zap } from '../../utils/zaps';
  import { createProfileStore } from '../../stores/profile';
  import { open as openZapModal } from '../Modals/ZapModal.svelte';

  interface Props {
    videoIdentifier: string;
    ownerPubkey: string;
    class?: string;
  }

  let { videoIdentifier, ownerPubkey, class: className = '' }: Props = $props();

  let allZaps = $state<Zap[]>([]);
  let zapCleanup = $state<(() => void) | null>(null);
  let totalSats = $derived(allZaps.reduce((sum, z) => sum + z.amountSats, 0));

  // Check if owner has lightning address
  let ownerNpub = $derived(ownerPubkey ? nip19.npubEncode(ownerPubkey) : '');
  let profileStore = $derived(ownerNpub ? createProfileStore(ownerNpub) : null);
  let profile = $state<{ lud16?: string; lud06?: string } | null>(null);
  let hasLightningAddress = $derived(!!profile?.lud16 || !!profile?.lud06);
  let canZap = $derived(hasLightningAddress); // Can zap if owner has lightning address (including yourself)
  let isDisabled = $derived(!hasLightningAddress); // Disabled if no lightning address

  $effect(() => {
    if (!profileStore) return;
    const unsub = profileStore.subscribe(value => {
      profile = value;
    });
    return unsub;
  });

  $effect(() => {
    const id = videoIdentifier;
    if (!id) return;

    untrack(() => {
      allZaps = [];
      zapCleanup = subscribeToZaps({ '#i': [id] }, (zap) => {
        allZaps = insertZapSorted(allZaps, zap);
      });
    });

    return () => {
      if (zapCleanup) {
        zapCleanup();
      }
    };
  });

  function handleZap() {
    if (canZap) {
      openZapModal(ownerPubkey, videoIdentifier);
    }
  }
</script>

<button
  onclick={handleZap}
  class="btn-ghost flex items-center gap-1 text-yellow-400 {className}"
  disabled={isDisabled}
  title={isDisabled ? 'No lightning address' : undefined}
  data-testid="zap-button"
>
  <span class="i-lucide-dollar-sign text-lg"></span>
  {#if totalSats > 0}
    <span class="font-semibold">{totalSats.toLocaleString()}</span>
  {:else if canZap}
    <span class="text-sm">Tip</span>
  {:else}
    <span class="text-sm">0</span>
  {/if}
</button>
