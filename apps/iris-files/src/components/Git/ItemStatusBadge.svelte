<script lang="ts">
  /**
   * Status badge for PRs and Issues
   */
  import type { ItemStatus } from '../../nip34';

  interface Props {
    status: ItemStatus;
    type?: 'pr' | 'issue';
  }

  let { status, type = 'issue' }: Props = $props();

  let statusConfig = $derived.by(() => {
    switch (status) {
      case 'open':
        return {
          icon: type === 'pr' ? 'i-lucide-git-pull-request' : 'i-lucide-circle-dot',
          color: 'text-success bg-success/10',
          label: 'Open',
        };
      case 'merged':
        return {
          icon: 'i-lucide-git-merge',
          color: 'text-purple-500 bg-purple-500/10',
          label: 'Merged',
        };
      case 'closed':
        return {
          icon: type === 'pr' ? 'i-lucide-git-pull-request-closed' : 'i-lucide-circle-x',
          color: 'text-danger bg-danger/10',
          label: 'Closed',
        };
      case 'draft':
        return {
          icon: 'i-lucide-file-edit',
          color: 'text-text-3 bg-surface-2',
          label: 'Draft',
        };
      default:
        return {
          icon: 'i-lucide-circle',
          color: 'text-text-3 bg-surface-2',
          label: status,
        };
    }
  });
</script>

<span class="inline-flex items-center gap-1 px-2 py-0.5 rounded-full text-xs font-medium {statusConfig.color}">
  <span class="{statusConfig.icon} text-sm"></span>
  {statusConfig.label}
</span>
