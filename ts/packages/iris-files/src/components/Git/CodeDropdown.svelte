<script lang="ts">
  /**
   * CodeDropdown - GitHub-style green "<> Code" button with clone instructions
   */
  import CopyInput from '../CopyInput.svelte';

  interface Props {
    npub: string;
    repoPath: string;
  }

  let { npub, repoPath }: Props = $props();

  let isOpen = $state(false);
  let buttonEl: HTMLButtonElement | undefined = $state();
  let dropdownEl: HTMLDivElement | undefined = $state();

  // Build the git clone command
  let cloneCommand = $derived(`git clone htree://${npub}/${repoPath}`);
  const rustCommand = "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh";
  const cargoCommand = "cargo install git-remote-htree";

  function handleClickOutside(event: MouseEvent) {
    const target = event.target as HTMLElement;
    if (!target.closest('.code-dropdown')) {
      isOpen = false;
    }
  }

  // Position dropdown to avoid overflow
  function positionDropdown() {
    if (!buttonEl || !dropdownEl) return;

    const buttonRect = buttonEl.getBoundingClientRect();
    const dropdownWidth = 420;
    const viewportWidth = window.innerWidth;
    const padding = 8;

    // Check if dropdown would overflow on the right when aligned left
    const wouldOverflowRight = buttonRect.left + dropdownWidth > viewportWidth - padding;
    // Check if dropdown would overflow on the left when aligned right
    const wouldOverflowLeft = buttonRect.right - dropdownWidth < padding;

    if (wouldOverflowRight && !wouldOverflowLeft) {
      // Align to right edge of button
      dropdownEl.style.right = '0';
      dropdownEl.style.left = 'auto';
    } else if (wouldOverflowLeft && !wouldOverflowRight) {
      // Align to left edge of button
      dropdownEl.style.left = '0';
      dropdownEl.style.right = 'auto';
    } else if (wouldOverflowLeft && wouldOverflowRight) {
      // Center in viewport
      dropdownEl.style.left = 'auto';
      dropdownEl.style.right = 'auto';
      dropdownEl.style.position = 'fixed';
      dropdownEl.style.left = `${Math.max(padding, (viewportWidth - dropdownWidth) / 2)}px`;
      dropdownEl.style.top = `${buttonRect.bottom + 4}px`;
    } else {
      // Default: align to right edge
      dropdownEl.style.right = '0';
      dropdownEl.style.left = 'auto';
    }
  }

  $effect(() => {
    if (isOpen) {
      document.addEventListener('click', handleClickOutside);
      // Position after render
      requestAnimationFrame(positionDropdown);
      return () => document.removeEventListener('click', handleClickOutside);
    }
  });
</script>

<div class="relative code-dropdown">
  <button
    bind:this={buttonEl}
    onclick={() => isOpen = !isOpen}
    class="flex items-center gap-1.5 px-3 h-8 rounded-md text-sm font-medium text-white bg-success hover:brightness-110 transition-all"
  >
    <span class="i-lucide-code text-base"></span>
    <span>Code</span>
    <span class="i-lucide-chevron-down text-xs ml-0.5"></span>
  </button>

  {#if isOpen}
    <div bind:this={dropdownEl} class="absolute right-0 top-full mt-1 w-[420px] max-w-[90vw] bg-surface-1 b-1 b-surface-3 b-solid rounded-lg shadow-lg z-50 overflow-hidden">
      <!-- Header -->
      <div class="px-3 py-2 bg-surface-2 b-b b-surface-3 b-solid">
        <span class="text-sm font-medium text-text-1">Clone</span>
      </div>

      <!-- Content -->
      <div class="p-3 space-y-3">
        <CopyInput text={cloneCommand} />

        <!-- Requirements -->
        <div class="space-y-2">
          <p class="text-xs text-text-3">
            Requires <a href="https://rustup.rs" target="_blank" rel="noopener" class="text-accent hover:underline">Rust</a> and <a href="#/npub1g53mukxnjkcmr94fhryzkqutdz2ukq4ks0gvy5af25rgmwsl4ngq43drvk/hashtree-rs" class="text-accent hover:underline">hashtree-cli</a>
          </p>
          <CopyInput text={rustCommand} />
          <CopyInput text={cargoCommand} />
        </div>
      </div>
    </div>
  {/if}
</div>
