<script lang="ts">
  /**
   * UploadProgress - Progress bar for uploads
   */

  interface Props {
    progress: number;
    message?: string;
  }

  let { progress, message = 'Processing...' }: Props = $props();
</script>

<div class="space-y-2">
  <div class="flex justify-between text-sm text-text-3">
    <span class="truncate flex-1 mr-2">{message}</span>
    <span>{progress}%</span>
  </div>
  <div class="h-2 bg-surface-2 rounded-full overflow-hidden">
    <div
      class="h-full bg-accent transition-all duration-300"
      style="width: {progress}%"
    ></div>
  </div>
</div>
