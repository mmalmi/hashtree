<script lang="ts">
  import { walletStore } from '../stores/wallet';
  import { getBtcUsdRate, satsToUsd } from '../utils/btcRate';

  let balance = $derived($walletStore.balance);
  let initialized = $derived($walletStore.initialized);
  let btcRate = $state<number | null>(null);

  // Fetch BTC rate on mount
  $effect(() => {
    getBtcUsdRate().then(rate => { btcRate = rate; }).catch(console.error);
  });

  // Format balance as USD
  function formatUsd(sats: number): string {
    if (!btcRate || sats === 0) return '$0';
    const usd = satsToUsd(sats, btcRate);
    if (usd < 0.01) return '<$0.01';
    return `$${usd.toFixed(2)}`;
  }
</script>

<a href="#/wallet" class="flex flex-col items-center text-text-2 hover:text-text-1 p-1 no-underline">
  <span class="i-lucide-wallet text-base"></span>
  {#if initialized}
    <span class="text-[10px] leading-none">{formatUsd(balance)}</span>
  {/if}
</a>
