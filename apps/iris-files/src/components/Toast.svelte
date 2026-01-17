<script lang="ts">
  /**
   * Toast - unified toast notification system
   * Shows upload progress and general notifications at bottom of screen
   */
  import { toasts, dismissToast, type ToastType } from '../stores/toast';
  import { uploadProgress, cancelUpload } from '../stores/upload';

  const iconMap: Record<ToastType, string> = {
    info: 'i-lucide-info',
    success: 'i-lucide-check-circle',
    error: 'i-lucide-x-circle',
    warning: 'i-lucide-alert-triangle',
  };

  const colorMap: Record<ToastType, string> = {
    info: 'text-accent',
    success: 'text-success',
    error: 'text-danger',
    warning: 'text-warning',
  };

  let percent = $derived(
    $uploadProgress?.totalBytes
      ? Math.round(($uploadProgress.bytes || 0) / $uploadProgress.totalBytes * 100)
      : $uploadProgress ? Math.round(($uploadProgress.current / $uploadProgress.total) * 100) : 0
  );

  let hasContent = $derived($uploadProgress || $toasts.length > 0);
</script>

{#if hasContent}
  <div
    class="pointer-events-none flex flex-col items-center gap-2"
    style="position: fixed; bottom: 1rem; left: 1rem; right: 1rem; z-index: 9999;"
  >
    <!-- Regular toasts -->
    {#each $toasts as toast (toast.id)}
      <div
        class="pointer-events-auto bg-surface-1 border border-surface-3 rounded-lg shadow-lg p-3 max-w-sm w-full flex items-start gap-2"
      >
        <span class="{iconMap[toast.type]} {colorMap[toast.type]} shrink-0 mt-0.5"></span>
        <span class="text-sm text-text-1 flex-1">{toast.message}</span>
        <button
          onclick={() => dismissToast(toast.id)}
          class="shrink-0 text-text-3 hover:text-text-1 transition-colors"
          aria-label="Dismiss"
        >
          <span class="i-lucide-x text-sm"></span>
        </button>
      </div>
    {/each}

    <!-- Upload progress toast -->
    {#if $uploadProgress}
      <div class="pointer-events-auto bg-surface-1 border border-accent rounded-lg shadow-lg p-3 max-w-sm w-full">
        <!-- Header with filename and cancel -->
        <div class="flex items-center gap-2 mb-2">
          <span class="i-lucide-loader-2 animate-spin text-accent shrink-0"></span>
          <span class="text-sm text-text-1 truncate flex-1">{$uploadProgress.fileName}</span>
          <button
            onclick={cancelUpload}
            class="btn-ghost shrink-0 px-2 py-0.5 text-xs"
          >
            Cancel
          </button>
        </div>

        <!-- Progress bar -->
        <div class="w-full h-1.5 bg-surface-3 rounded-full overflow-hidden mb-1">
          <div
            class="h-full bg-accent transition-all duration-150"
            style="width: {percent}%"
          ></div>
        </div>

        <!-- Status and count -->
        <div class="flex items-center justify-between text-xs text-text-3">
          <span class="capitalize">{$uploadProgress.status || 'uploading'}...</span>
          <span>{$uploadProgress.current} / {$uploadProgress.total}</span>
        </div>
      </div>
    {/if}
  </div>
{/if}
