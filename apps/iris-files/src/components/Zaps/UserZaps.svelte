<script lang="ts">
  /**
   * UserZaps - Self-contained component for showing zaps to a user
   * Handles subscription and display
   */
  import { untrack } from 'svelte';
  import { subscribeToZaps, insertZapSorted, type Zap } from '../../utils/zaps';
  import ZapsList from './ZapsList.svelte';

  interface Props {
    pubkey: string;
    showSummary?: boolean;
    collapsible?: boolean;
    maxItems?: number;
  }

  let { pubkey, showSummary = true, collapsible = false, maxItems = 50 }: Props = $props();

  let allZaps = $state<Zap[]>([]);
  let zapCleanup = $state<(() => void) | null>(null);

  $effect(() => {
    const pk = pubkey;
    if (!pk) return;

    untrack(() => {
      allZaps = [];
      zapCleanup = subscribeToZaps({ '#p': [pk] }, (zap) => {
        allZaps = insertZapSorted(allZaps, zap);
      });
    });

    return () => {
      if (zapCleanup) {
        zapCleanup();
      }
    };
  });
</script>

<ZapsList zaps={allZaps} {showSummary} {collapsible} {maxItems} />
