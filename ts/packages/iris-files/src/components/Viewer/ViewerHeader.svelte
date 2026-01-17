<script lang="ts">
  /**
   * ViewerHeader - Shared header for viewer panel
   * Shows back button, avatar, visibility, and name
   * Used by directory viewer (git repos, regular dirs)
   */
  import type { CID, TreeVisibility } from 'hashtree';
  import type { Snippet } from 'svelte';
  import { npubToPubkey } from '../../nostr';
  import { Avatar } from '../User';
  import VisibilityIcon from '../VisibilityIcon.svelte';

  interface Props {
    backUrl: string;
    npub?: string | null;
    isPermalink?: boolean;
    rootCid?: CID | null;
    visibility?: TreeVisibility;
    icon: string;
    name: string;
    /** Additional classes for outer container */
    class?: string;
    children?: Snippet;
  }

  let {
    backUrl,
    npub = null,
    isPermalink = false,
    rootCid = null,
    visibility,
    icon,
    name,
    class: className = '',
    children,
  }: Props = $props();
</script>

<div class="shrink-0 px-3 py-2 border-b border-surface-2 flex items-center gap-2 {className}">
  <a href={backUrl} class="btn-ghost p-1 no-underline inline-flex items-center justify-center" title="Back">
    <span class="i-lucide-chevron-left text-lg"></span>
  </a>
  <!-- Avatar (for npub routes) or LinkLock/globe (for nhash routes) -->
  {#if npub}
    <a href="#/{npub}/profile" class="shrink-0 inline-flex items-center">
      <Avatar pubkey={npubToPubkey(npub) || ''} size={20} />
    </a>
  {:else if isPermalink}
    {#if rootCid?.key}
      <!-- LinkLockIcon for encrypted permalink -->
      <span class="relative inline-flex items-center shrink-0 text-text-2" title="Encrypted permalink">
        <span class="i-lucide-link"></span>
        <span class="i-lucide-lock absolute -bottom-0.5 -right-1.5 text-[0.6em]"></span>
      </span>
    {:else}
      <span class="i-lucide-globe text-text-2 shrink-0" title="Public permalink"></span>
    {/if}
  {/if}
  <!-- Visibility icon -->
  {#if visibility}
    <VisibilityIcon {visibility} class="text-text-2" />
  {/if}
  <!-- Icon + name -->
  <span class="{icon} shrink-0"></span>
  <span class="font-medium text-text-1 truncate leading-none">{name}</span>
  <!-- Slot for additional content (like LIVE badge) -->
  {#if children}
    {@render children()}
  {/if}
</div>
