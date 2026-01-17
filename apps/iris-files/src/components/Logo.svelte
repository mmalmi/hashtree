<script lang="ts">
  let { class: className = '', app = 'files' }: { class?: string; app?: string } = $props();
</script>

<span class="flex items-center gap-2 {className}">
  <img src="/iris-logo.png" alt="iris" class="w-8 h-8" draggable="false" />
  <span class="font-semibold text-xl text-text-1">iris <span class="text-accent">{app}</span></span>
</span>
