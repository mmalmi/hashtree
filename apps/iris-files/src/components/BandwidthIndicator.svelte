<script lang="ts">
  /**
   * Bandwidth indicator - shows current upload/download rates in header
   */
  import { getWorkerAdapter } from '../lib/workerInit';

  // Track previous bytes for rate calculation
  let prevBytes = $state({ sent: 0, received: 0, time: Date.now() });
  let rates = $state({ up: 0, down: 0 });

  // Update rates periodically
  $effect(() => {
    const interval = setInterval(async () => {
      const adapter = getWorkerAdapter();
      if (!adapter) return;

      try {
        const stats = await adapter.getPeerStats();
        const now = Date.now();
        const elapsed = (now - prevBytes.time) / 1000; // seconds

        // Sum up bytes from all peers
        const totalSent = stats.reduce((sum, p) => sum + p.bytesSent, 0);
        const totalReceived = stats.reduce((sum, p) => sum + p.bytesReceived, 0);

        if (elapsed > 0 && prevBytes.time > 0) {
          const sentDiff = totalSent - prevBytes.sent;
          const receivedDiff = totalReceived - prevBytes.received;

          rates = {
            up: Math.max(0, sentDiff / elapsed),
            down: Math.max(0, receivedDiff / elapsed),
          };
        }

        prevBytes = {
          sent: totalSent,
          received: totalReceived,
          time: now,
        };
      } catch {
        // Worker not ready
      }
    }, 1000);

    return () => clearInterval(interval);
  });

  function formatRate(bytesPerSec: number): string {
    if (bytesPerSec < 1024) return `${Math.round(bytesPerSec)} B/s`;
    if (bytesPerSec < 1024 * 1024) return `${(bytesPerSec / 1024).toFixed(1)} KB/s`;
    return `${(bytesPerSec / (1024 * 1024)).toFixed(1)} MB/s`;
  }
</script>

<a
  href="#/settings/p2p"
  class="flex flex-col items-end text-xs no-underline font-mono leading-tight"
  title="Upload: {formatRate(rates.up)}, Download: {formatRate(rates.down)}"
>
  <span class="flex items-center gap-0.5" class:text-green-400={rates.up > 0} class:text-text-3={rates.up === 0}>
    <span class="i-lucide-arrow-up text-xs"></span>
    <span>{formatRate(rates.up)}</span>
  </span>
  <span class="flex items-center gap-0.5" class:text-blue-400={rates.down > 0} class:text-text-3={rates.down === 0}>
    <span class="i-lucide-arrow-down text-xs"></span>
    <span>{formatRate(rates.down)}</span>
  </span>
</a>
