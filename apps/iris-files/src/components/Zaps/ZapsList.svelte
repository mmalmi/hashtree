<script lang="ts">
  /**
   * ZapsList - displays zaps with summary and individual items
   * Reusable component for video comments and profile pages
   */
  import { nip19 } from 'nostr-tools';
  import { Avatar, Name } from '../User';
  import type { Zap } from '../../utils/zaps';

  interface Props {
    zaps: Zap[];
    showSummary?: boolean;
    collapsible?: boolean;
    maxItems?: number;
  }

  let { zaps, showSummary = true, collapsible = false, maxItems = 50 }: Props = $props();

  let expanded = $state(false);
  let totalSats = $derived(zaps.reduce((sum, z) => sum + z.amountSats, 0));
  let displayZaps = $derived(maxItems ? zaps.slice(0, maxItems) : zaps);

  // Throttled total for smoother display
  let displayedTotal = $state(0);
  let throttleTimeout: ReturnType<typeof setTimeout> | null = null;

  $effect(() => {
    const newTotal = totalSats;
    if (displayedTotal === 0) {
      // First load - show immediately
      displayedTotal = newTotal;
    } else if (throttleTimeout === null) {
      // Throttle updates to every 500ms
      throttleTimeout = setTimeout(() => {
        displayedTotal = newTotal;
        throttleTimeout = null;
      }, 500);
    }
  });

  function formatTime(timestamp: number): string {
    const date = new Date(timestamp * 1000);
    const now = new Date();
    const diff = now.getTime() - date.getTime();

    const minutes = Math.floor(diff / 60000);
    if (minutes < 1) return 'just now';
    if (minutes < 60) return `${minutes}m ago`;

    const hours = Math.floor(minutes / 60);
    if (hours < 24) return `${hours}h ago`;

    const days = Math.floor(hours / 24);
    if (days < 7) return `${days}d ago`;

    return date.toLocaleDateString();
  }
</script>

{#if zaps.length > 0}
  <!-- Summary -->
  {#if showSummary}
    <button
      class="flex items-center gap-3 p-3 bg-surface-1 rounded-lg w-full text-left {collapsible ? 'cursor-pointer hover:bg-surface-2' : 'cursor-default'}"
      data-testid="zaps-summary"
      onclick={() => collapsible && (expanded = !expanded)}
      disabled={!collapsible}
    >
      <span class="i-lucide-dollar-sign text-yellow-400 text-xl"></span>
      <div class="flex-1">
        <span class="font-semibold text-yellow-400" data-testid="zaps-total">
          $ {displayedTotal.toLocaleString()} sats
        </span>
        <span class="text-text-3 text-sm ml-2">
          from {zaps.length} tip{zaps.length !== 1 ? 's' : ''}
        </span>
      </div>
      {#if collapsible}
        <span class="i-lucide-chevron-down text-text-3 transition-transform {expanded ? 'rotate-180' : ''}"></span>
      {/if}
    </button>
  {/if}

  <!-- List -->
  {#if !collapsible || expanded}
  <div class="space-y-3 {collapsible ? 'mt-3 max-h-64 overflow-y-auto' : ''}" data-testid="zaps-list">
    {#each displayZaps as zap (zap.id)}
      <div class="flex gap-3 p-3 bg-surface-1 rounded-lg" data-testid="zap-item">
        <a href={`#/${nip19.npubEncode(zap.senderPubkey)}`} class="shrink-0">
          <Avatar pubkey={zap.senderPubkey} size={36} />
        </a>
        <div class="flex-1 min-w-0">
          <div class="flex items-center gap-2 flex-wrap">
            <a href={`#/${nip19.npubEncode(zap.senderPubkey)}`} class="font-medium text-text-1 hover:text-accent no-underline">
              <Name pubkey={zap.senderPubkey} />
            </a>
            <span class="text-yellow-400 font-semibold">
              $ {zap.amountSats.toLocaleString()}
            </span>
            <span class="text-xs text-text-3">{formatTime(zap.createdAt)}</span>
          </div>
          {#if zap.comment}
            <p class="text-text-2 text-sm mt-1 whitespace-pre-wrap break-words">{zap.comment}</p>
          {/if}
        </div>
      </div>
    {/each}
  </div>
  {/if}
{/if}
