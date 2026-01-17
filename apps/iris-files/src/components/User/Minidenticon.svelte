<script lang="ts">
  import { minidenticon } from 'minidenticons';

  interface Props {
    seed: string;
    size?: number;
    saturation?: number;
    lightness?: number;
    class?: string;
  }

  let { seed, size = 40, saturation = 50, lightness = 50, class: className = '' }: Props = $props();

  let svgURI = $derived('data:image/svg+xml;utf8,' + encodeURIComponent(minidenticon(seed, saturation, lightness)));
</script>

<div
  class="rounded-full flex items-center justify-center shrink-0 bg-surface-3 {className}"
  style="width: {size}px; height: {size}px;"
>
  <img
    src={svgURI}
    alt=""
    width={size}
    height={size}
  />
</div>
