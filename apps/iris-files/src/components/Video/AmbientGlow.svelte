<script lang="ts">
  /**
   * AmbientGlow - YouTube-style ambient mode effect
   * Extracts colors from video and creates a soft glow that bleeds into surroundings
   */
  import { ambientColor } from '../../stores/ambientGlow';

  interface Props {
    videoRef: HTMLVideoElement | undefined;
  }

  let { videoRef }: Props = $props();

  let canvas: HTMLCanvasElement | undefined = $state();
  let animationFrame: number | null = null;
  let lastUpdate = 0;
  const UPDATE_INTERVAL = 500; // Update every 500ms for performance

  function extractDominantColor() {
    if (!videoRef || !canvas || videoRef.paused || videoRef.ended) return;

    const ctx = canvas.getContext('2d', { willReadFrequently: true });
    if (!ctx) return;

    // Draw current video frame to canvas (small size for performance)
    const w = 32;
    const h = 18; // 16:9 aspect
    canvas.width = w;
    canvas.height = h;

    try {
      ctx.drawImage(videoRef, 0, 0, w, h);
      const imageData = ctx.getImageData(0, 0, w, h);
      const data = imageData.data;

      // Sample colors and compute weighted average (favor saturated colors)
      let r = 0, g = 0, b = 0, count = 0;

      for (let i = 0; i < data.length; i += 16) { // Sample every 4th pixel
        const pr = data[i];
        const pg = data[i + 1];
        const pb = data[i + 2];

        // Calculate saturation (skip very dark or very light pixels)
        const max = Math.max(pr, pg, pb);
        const min = Math.min(pr, pg, pb);
        const lightness = (max + min) / 2;

        if (lightness > 20 && lightness < 235) {
          // Weight by saturation for more vibrant colors
          const saturation = max === 0 ? 0 : (max - min) / max;
          const weight = 1 + saturation * 2;

          r += pr * weight;
          g += pg * weight;
          b += pb * weight;
          count += weight;
        }
      }

      if (count > 0) {
        r = Math.round(r / count);
        g = Math.round(g / count);
        b = Math.round(b / count);

        // Boost saturation slightly for more vivid glow
        const max = Math.max(r, g, b);
        const min = Math.min(r, g, b);
        if (max > min) {
          const boost = 1.3;
          r = Math.min(255, Math.round(r + (r - (r + g + b) / 3) * (boost - 1)));
          g = Math.min(255, Math.round(g + (g - (r + g + b) / 3) * (boost - 1)));
          b = Math.min(255, Math.round(b + (b - (r + g + b) / 3) * (boost - 1)));
        }

        ambientColor.set({ r, g, b });
      }
    } catch {
      // CORS or other error - silently ignore
    }
  }

  function updateLoop() {
    const now = performance.now();
    if (now - lastUpdate >= UPDATE_INTERVAL) {
      extractDominantColor();
      lastUpdate = now;
    }
    animationFrame = requestAnimationFrame(updateLoop);
  }

  // Start/stop loop based on video state
  $effect(() => {
    if (!videoRef) return;

    function handlePlay() {
      if (!animationFrame) {
        updateLoop();
      }
    }

    function handlePause() {
      if (animationFrame) {
        cancelAnimationFrame(animationFrame);
        animationFrame = null;
      }
    }

    function handleSeeked() {
      extractDominantColor();
    }

    videoRef.addEventListener('play', handlePlay);
    videoRef.addEventListener('pause', handlePause);
    videoRef.addEventListener('ended', handlePause);
    videoRef.addEventListener('seeked', handleSeeked);
    videoRef.addEventListener('loadeddata', handleSeeked);

    // Start if already playing
    if (!videoRef.paused) {
      handlePlay();
    }

    return () => {
      videoRef.removeEventListener('play', handlePlay);
      videoRef.removeEventListener('pause', handlePause);
      videoRef.removeEventListener('ended', handlePause);
      videoRef.removeEventListener('seeked', handleSeeked);
      videoRef.removeEventListener('loadeddata', handleSeeked);

      if (animationFrame) {
        cancelAnimationFrame(animationFrame);
        animationFrame = null;
      }
      // Clear ambient color when unmounting
      ambientColor.set(null);
    };
  });
</script>

<!-- Hidden canvas for color extraction -->
<canvas bind:this={canvas} class="hidden"></canvas>
