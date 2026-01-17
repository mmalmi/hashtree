<script lang="ts">
  /**
   * ProxyImg - Image component with imgproxy support
   * Proxies external images through imgproxy for performance and privacy
   * Falls back to original URL on error
   */
  import { generateProxyUrl, DEFAULT_IMGPROXY_CONFIG, type ImgProxyOptions } from '../utils/imgproxy';
  import { settingsStore } from '../stores/settings';

  interface Props {
    src: string;
    alt?: string;
    width?: number;
    height?: number;
    class?: string;
    style?: string;
    /** If true, use fill mode (crop to fill); if false, use fit mode (contain) */
    square?: boolean;
    /** Whether to use proxy (can be disabled) */
    useProxy?: boolean;
    /** Callback when image loads */
    onload?: () => void;
    /** Callback when image fails to load */
    onerror?: () => void;
  }

  let {
    src,
    alt = '',
    width,
    height,
    class: className = '',
    style = '',
    square = false,
    useProxy = true,
    onload,
    onerror,
  }: Props = $props();

  let hasError = $state(false);

  // Get imgproxy config from settings
  let imgproxySettings = $derived($settingsStore.imgproxy);
  let proxyEnabled = $derived(imgproxySettings.enabled && useProxy);

  let imgproxyConfig = $derived.by(() => {
    if (imgproxySettings.url && imgproxySettings.key && imgproxySettings.salt) {
      return {
        url: imgproxySettings.url,
        key: imgproxySettings.key,
        salt: imgproxySettings.salt,
      };
    }
    return DEFAULT_IMGPROXY_CONFIG;
  });

  // Reset error state when src changes
  $effect(() => {
    src; // Track src changes
    hasError = false;
  });

  // Generate the current src URL
  let currentSrc = $derived.by(() => {
    if (!src || !proxyEnabled || hasError) {
      return src;
    }

    const options: ImgProxyOptions = {
      width,
      height,
      square,
    };

    return generateProxyUrl(src, options, imgproxyConfig);
  });

  function handleError() {
    if (!hasError && currentSrc !== src) {
      // Fallback to original URL on proxy error
      hasError = true;
    }
    onerror?.();
  }

  function handleLoad() {
    onload?.();
  }
</script>

<img
  src={currentSrc}
  {alt}
  width={width}
  height={height}
  class={className}
  style={style}
  onload={handleLoad}
  onerror={handleError}
/>
