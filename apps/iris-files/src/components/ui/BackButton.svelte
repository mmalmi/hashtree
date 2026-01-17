<script lang="ts">
  /**
   * BackButton - consistent back navigation button with chevron icon
   */
  import { navigate } from '../../utils/navigate';

  interface Props {
    href?: string;
    label?: string;
    onclick?: () => void;
    /** Use browser history.back() if available, fallback to href */
    useHistory?: boolean;
    class?: string;
  }

  let { href = '/', label, onclick, useHistory = false, class: className = '' }: Props = $props();

  function handleClick() {
    if (onclick) {
      onclick();
    } else if (useHistory && window.history.length > 1) {
      // Check if we have history to go back to
      window.history.back();
    } else {
      navigate(href);
    }
  }
</script>

<button
  onclick={handleClick}
  class="btn-ghost flex items-center gap-1 {className}"
  type="button"
>
  <span class="i-lucide-chevron-left"></span>
  {#if label}
    <span>{label}</span>
  {/if}
</button>
