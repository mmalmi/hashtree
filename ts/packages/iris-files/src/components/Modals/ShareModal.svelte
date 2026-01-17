<script lang="ts" module>
  /**
   * ShareModal - unified sharing options with QR code, copy link, and native share
   */
  let show = $state(false);
  let url = $state<string | null>(null);

  export function open(shareUrl: string) {
    url = shareUrl;
    show = true;
  }

  export function close() {
    show = false;
    url = null;
  }
</script>

<script lang="ts">
  import QRCode from 'qrcode';
  import CopyText from '../CopyText.svelte';

  let qrDataUrl = $state<string | null>(null);

  // Generate QR code when modal opens
  $effect(() => {
    if (!show || !url) {
      qrDataUrl = null;
      return;
    }
    generateQrCode(url).then((u) => (qrDataUrl = u));
  });

  // Handle Escape key to close modal
  $effect(() => {
    if (!show) return;

    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        (document.activeElement as HTMLElement)?.blur();
        close();
      }
    };

    document.addEventListener('keydown', handleKeyDown);
    return () => document.removeEventListener('keydown', handleKeyDown);
  });

  async function handleNativeShare() {
    if (navigator.share && url) {
      try {
        await navigator.share({ url });
      } catch (e) {
        if ((e as Error).name !== 'AbortError') {
          console.error('Share failed:', e);
        }
      }
    }
  }

  async function generateQrCode(text: string): Promise<string> {
    return QRCode.toDataURL(text, {
      width: 200,
      margin: 2,
      color: { dark: '#000000', light: '#ffffff' },
    });
  }
</script>

{#if show && url}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div
    class="fixed inset-0 bg-black/70 flex-center z-1000 overflow-auto"
    onclick={(e) => {
      if (e.target === e.currentTarget) close();
    }}
    data-testid="share-modal-backdrop"
  >
    <div
      class="bg-surface-1 sm:rounded-lg overflow-auto w-screen sm:w-96 sm:border border-surface-3 max-h-full my-auto"
      data-testid="share-modal"
    >
      <!-- QR Code - click to close -->
      <div class="cursor-pointer" onclick={close}>
        {#if qrDataUrl}
          <img
            src={qrDataUrl}
            alt="QR Code"
            class="w-full max-h-screen object-contain bg-white"
            data-testid="share-qr-code"
          />
        {:else}
          <div class="w-full aspect-square max-h-screen bg-surface-2 flex-center">
            <span class="i-lucide-loader-2 animate-spin text-2xl text-text-3"></span>
          </div>
        {/if}
      </div>

      <!-- URL with copy -->
      <div class="bg-surface-2 p-3 m-4 mb-2 rounded">
        <CopyText text={url} truncate={80} class="text-sm" testId="share-copy-url" />
      </div>

      <!-- Native share button -->
      {#if typeof navigator !== 'undefined' && 'share' in navigator}
        <div class="px-4 pb-4 pt-2">
          <button onclick={handleNativeShare} class="btn-ghost w-full flex items-center justify-center gap-2">
            <span class="i-lucide-share"></span>
            Share via...
          </button>
        </div>
      {/if}
    </div>
  </div>
{/if}
