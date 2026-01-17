<script lang="ts">
  /**
   * CI Status badge for displaying build/test status
   */
  import type { CIJobStatus } from '../../stores/ci';

  interface Props {
    status: CIJobStatus | null;
    /** Show compact version (icon only) */
    compact?: boolean;
    /** Optional job name to display */
    jobName?: string;
  }

  let { status, compact = false, jobName }: Props = $props();

  let statusConfig = $derived.by(() => {
    switch (status) {
      case 'success':
        return {
          icon: 'i-lucide-check-circle',
          color: 'text-success bg-success/10',
          label: 'Passed',
        };
      case 'failure':
        return {
          icon: 'i-lucide-x-circle',
          color: 'text-danger bg-danger/10',
          label: 'Failed',
        };
      case 'running':
        return {
          icon: 'i-lucide-loader-2 animate-spin',
          color: 'text-warning bg-warning/10',
          label: 'Running',
        };
      case 'queued':
        return {
          icon: 'i-lucide-clock',
          color: 'text-text-3 bg-surface-2',
          label: 'Queued',
        };
      case 'cancelled':
        return {
          icon: 'i-lucide-ban',
          color: 'text-text-3 bg-surface-2',
          label: 'Cancelled',
        };
      case 'skipped':
        return {
          icon: 'i-lucide-skip-forward',
          color: 'text-text-3 bg-surface-2',
          label: 'Skipped',
        };
      default:
        return null;
    }
  });
</script>

{#if statusConfig}
  {#if compact}
    <span
      class="{statusConfig.icon} text-sm {statusConfig.color.split(' ')[0]}"
      title="{jobName ? `${jobName}: ` : ''}{statusConfig.label}"
      data-testid="ci-status-badge"
      data-ci-status={status}
    ></span>
  {:else}
    <span
      class="inline-flex items-center gap-1 px-2 py-0.5 rounded-full text-xs font-medium {statusConfig.color}"
      data-testid="ci-status-badge"
      data-ci-status={status}
    >
      <span class="{statusConfig.icon} text-sm"></span>
      {#if jobName}
        <span class="text-text-3">{jobName}:</span>
      {/if}
      {statusConfig.label}
    </span>
  {/if}
{/if}
