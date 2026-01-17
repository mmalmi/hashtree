<script lang="ts">
  /**
   * QR Scanner component for scanning npubs from QR codes
   * Based on React implementation using jsQR
   */
  import { onMount, onDestroy } from 'svelte';
  import jsQR from 'jsqr';

  interface Props {
    onScanSuccess: (result: string) => void;
    onClose: () => void;
  }

  let { onScanSuccess, onClose }: Props = $props();

  let videoElement: HTMLVideoElement | undefined = $state();
  let canvasElement: HTMLCanvasElement | undefined = $state();
  let stream: MediaStream | null = $state(null);
  let animationId: number | null = $state(null);
  let error = $state('');

  function startScanning() {
    if (!canvasElement || !videoElement) return;

    const ctx = canvasElement.getContext('2d', { willReadFrequently: true });
    if (!ctx) return;

    // Check for camera support
    if (!navigator.mediaDevices?.getUserMedia) {
      error = 'Camera access not supported in this browser';
      return;
    }

    navigator.mediaDevices
      .getUserMedia({
        video: { facingMode: 'environment' }, // Use back camera if available
      })
      .then((mediaStream) => {
        stream = mediaStream;
        if (videoElement) {
          videoElement.srcObject = mediaStream;
          videoElement.play();
        }

        const scanQRCode = () => {
          if (!videoElement || !canvasElement || !ctx) return;

          if (videoElement.readyState === videoElement.HAVE_ENOUGH_DATA) {
            // Set canvas dimensions to match video
            canvasElement.height = videoElement.videoHeight;
            canvasElement.width = videoElement.videoWidth;

            // Draw current video frame to canvas
            ctx.drawImage(videoElement, 0, 0, canvasElement.width, canvasElement.height);

            // Get image data for QR processing
            const imageData = ctx.getImageData(0, 0, canvasElement.width, canvasElement.height);

            // Process with jsQR
            const code = jsQR(imageData.data, imageData.width, imageData.height, {
              inversionAttempts: 'dontInvert', // Faster processing
            });

            if (code) {
              // QR code found - extract npub from scanned text
              const text = code.data;
              onScanSuccess(text);
              return; // Stop scanning after success
            }
          }

          // Continue scanning
          animationId = requestAnimationFrame(scanQRCode);
        };

        scanQRCode();
      })
      .catch((err) => {
        console.error('Error accessing camera:', err);
        error = 'Unable to access camera. Please make sure you have granted camera permissions.';
      });
  }

  function cleanup() {
    if (animationId !== null) {
      cancelAnimationFrame(animationId);
      animationId = null;
    }
    if (stream) {
      stream.getTracks().forEach((track) => track.stop());
      stream = null;
    }
  }

  onMount(() => {
    startScanning();
  });

  onDestroy(() => {
    cleanup();
  });

  function handleClose() {
    cleanup();
    onClose();
  }
</script>

<div class="fixed inset-0 bg-black/90 flex items-center justify-center z-[1010]">
  <div class="relative w-full max-w-md mx-4">
    <!-- Close button -->
    <button
      onclick={handleClose}
      class="absolute -top-12 right-0 text-white hover:text-accent"
      aria-label="Close scanner"
    >
      <span class="i-lucide-x text-2xl"></span>
    </button>

    <!-- Scanner viewport -->
    <div class="bg-surface-1 rounded-lg overflow-hidden">
      <div class="p-3 border-b border-surface-3">
        <h3 class="text-lg font-semibold">Scan QR Code</h3>
        <p class="text-sm text-text-3">Point camera at a QR code containing an npub</p>
      </div>

      <div class="relative aspect-square">
        {#if error}
          <div class="flex items-center justify-center h-full p-4">
            <p class="text-danger text-center">{error}</p>
          </div>
        {:else}
          <video
            bind:this={videoElement}
            class="w-full h-full object-cover"
            playsinline
          ></video>
          <canvas bind:this={canvasElement} class="hidden"></canvas>

          <!-- Scan overlay with corner markers -->
          <div class="absolute inset-0 flex items-center justify-center pointer-events-none">
            <div class="w-2/3 h-2/3 relative">
              <!-- Corner markers -->
              <div class="absolute top-0 left-0 w-8 h-8 border-t-2 border-l-2 border-accent"></div>
              <div class="absolute top-0 right-0 w-8 h-8 border-t-2 border-r-2 border-accent"></div>
              <div class="absolute bottom-0 left-0 w-8 h-8 border-b-2 border-l-2 border-accent"></div>
              <div class="absolute bottom-0 right-0 w-8 h-8 border-b-2 border-r-2 border-accent"></div>
            </div>
          </div>
        {/if}
      </div>

      <div class="p-3 border-t border-surface-3">
        <button onclick={handleClose} class="btn-ghost w-full">
          Cancel
        </button>
      </div>
    </div>
  </div>
</div>
