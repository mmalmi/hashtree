/**
 * Document thumbnail capture utility
 * Captures a preview image of the document editor content
 */

const THUMBNAIL_WIDTH = 300;
const THUMBNAIL_HEIGHT = 424; // A4 aspect ratio (1:1.414)
const THUMBNAIL_FILENAME = '.thumbnail.jpg';

/**
 * Capture a thumbnail of an element
 * Returns JPEG data as Uint8Array, or null if capture fails
 */
export async function captureThumbnail(element: HTMLElement): Promise<Uint8Array | null> {
  try {
    // Dynamically import html2canvas to avoid loading it until needed
    const html2canvas = (await import('html2canvas')).default;

    // Capture the element
    const canvas = await html2canvas(element, {
      scale: 1, // Full resolution for better quality
      logging: false,
      useCORS: true,
      allowTaint: true,
      backgroundColor: '#161b22', // Match surface-1 background
      width: Math.min(element.scrollWidth, 800),
      height: Math.min(element.scrollHeight, 1200),
    });

    // Create a smaller canvas for the thumbnail
    const thumbCanvas = document.createElement('canvas');
    thumbCanvas.width = THUMBNAIL_WIDTH;
    thumbCanvas.height = THUMBNAIL_HEIGHT;
    const ctx = thumbCanvas.getContext('2d');
    if (!ctx) return null;

    // Fill with background color
    ctx.fillStyle = '#161b22';
    ctx.fillRect(0, 0, THUMBNAIL_WIDTH, THUMBNAIL_HEIGHT);

    // Calculate scaling to fit while maintaining aspect ratio
    const sourceAspect = canvas.width / canvas.height;
    const targetAspect = THUMBNAIL_WIDTH / THUMBNAIL_HEIGHT;

    let drawWidth = THUMBNAIL_WIDTH;
    let drawHeight = THUMBNAIL_HEIGHT;
    let offsetX = 0;
    let offsetY = 0;

    if (sourceAspect > targetAspect) {
      // Source is wider - fit to width, crop height
      drawHeight = THUMBNAIL_WIDTH / sourceAspect;
      offsetY = 0; // Align to top
    } else {
      // Source is taller - fit to height, crop width
      drawWidth = THUMBNAIL_HEIGHT * sourceAspect;
      offsetX = (THUMBNAIL_WIDTH - drawWidth) / 2; // Center horizontally
    }

    // Draw the captured content onto the thumbnail canvas
    ctx.drawImage(canvas, offsetX, offsetY, drawWidth, drawHeight);

    // Convert to JPEG blob (smaller file size than PNG)
    const blob = await new Promise<Blob | null>((resolve) => {
      thumbCanvas.toBlob(resolve, 'image/jpeg', 0.85);
    });

    if (!blob) return null;

    // Convert blob to Uint8Array
    const arrayBuffer = await blob.arrayBuffer();
    return new Uint8Array(arrayBuffer);
  } catch (error) {
    console.error('[thumbnail] Failed to capture:', error);
    return null;
  }
}

/**
 * Get the thumbnail filename
 */
export function getThumbnailFilename(): string {
  return THUMBNAIL_FILENAME;
}

/**
 * Throttle thumbnail capture to avoid too frequent captures
 * Returns a function that captures at most once per interval
 */
export function createThrottledCapture(intervalMs: number = 30000) {
  let lastCapture = 0;
  let pendingCapture: Promise<Uint8Array | null> | null = null;

  const capture = async (element: HTMLElement): Promise<Uint8Array | null> => {
    const now = Date.now();

    // If we captured recently, skip
    if (now - lastCapture < intervalMs) {
      return null;
    }

    // If a capture is already in progress, wait for it
    if (pendingCapture) {
      return pendingCapture;
    }

    lastCapture = now;
    pendingCapture = captureThumbnail(element);

    try {
      return await pendingCapture;
    } finally {
      pendingCapture = null;
    }
  };

  // Expose reset for testing
  capture.reset = () => {
    lastCapture = 0;
  };

  return capture;
}
