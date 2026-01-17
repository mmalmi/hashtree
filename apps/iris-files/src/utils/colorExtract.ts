/**
 * Extract dominant color from an image URL
 * Used for thumbnail-based theme colors in video cards and player
 */

export interface RGB {
  r: number;
  g: number;
  b: number;
}

/** Convert RGB to HSL */
function rgbToHsl(r: number, g: number, b: number): [number, number, number] {
  r /= 255; g /= 255; b /= 255;
  const max = Math.max(r, g, b), min = Math.min(r, g, b);
  const l = (max + min) / 2;
  let h = 0, s = 0;

  if (max !== min) {
    const d = max - min;
    s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
    switch (max) {
      case r: h = ((g - b) / d + (g < b ? 6 : 0)) / 6; break;
      case g: h = ((b - r) / d + 2) / 6; break;
      case b: h = ((r - g) / d + 4) / 6; break;
    }
  }
  return [h, s, l];
}

/** Convert HSL to RGB */
function hslToRgb(h: number, s: number, l: number): RGB {
  let r, g, b;

  if (s === 0) {
    r = g = b = l;
  } else {
    const hue2rgb = (p: number, q: number, t: number) => {
      if (t < 0) t += 1;
      if (t > 1) t -= 1;
      if (t < 1/6) return p + (q - p) * 6 * t;
      if (t < 1/2) return q;
      if (t < 2/3) return p + (q - p) * (2/3 - t) * 6;
      return p;
    };
    const q = l < 0.5 ? l * (1 + s) : l + s - l * s;
    const p = 2 * l - q;
    r = hue2rgb(p, q, h + 1/3);
    g = hue2rgb(p, q, h);
    b = hue2rgb(p, q, h - 1/3);
  }

  return {
    r: Math.round(r * 255),
    g: Math.round(g * 255),
    b: Math.round(b * 255)
  };
}

/**
 * Extract the dominant hue from an image with fixed saturation/lightness
 * @param url - Image URL to extract color from
 * @param saturation - Fixed saturation (0-1, default 0.7)
 * @param lightness - Fixed lightness (0-1, default 0.5)
 * @returns Promise resolving to RGB color or null on error
 */
export function extractDominantColor(url: string, saturation = 0.7, lightness = 0.5): Promise<RGB | null> {
  return new Promise((resolve) => {
    const img = new Image();
    img.crossOrigin = 'anonymous';

    img.onload = () => {
      const canvas = document.createElement('canvas');
      const ctx = canvas.getContext('2d', { willReadFrequently: true });
      if (!ctx) {
        resolve(null);
        return;
      }

      const w = 16, h = 9;
      canvas.width = w;
      canvas.height = h;

      try {
        ctx.drawImage(img, 0, 0, w, h);
        const data = ctx.getImageData(0, 0, w, h).data;

        // Accumulate hue weighted by saturation
        let hueX = 0, hueY = 0, count = 0;

        for (let i = 0; i < data.length; i += 16) {
          const pr = data[i], pg = data[i + 1], pb = data[i + 2];
          const [h, s, l] = rgbToHsl(pr, pg, pb);

          // Skip very dark, very light, or desaturated pixels
          if (l > 0.1 && l < 0.9 && s > 0.1) {
            // Weight by saturation for more vibrant hues
            const weight = s * s;
            // Use circular mean for hue (avoid 0/360 boundary issues)
            hueX += Math.cos(h * 2 * Math.PI) * weight;
            hueY += Math.sin(h * 2 * Math.PI) * weight;
            count += weight;
          }
        }

        if (count > 0) {
          // Calculate average hue using atan2
          const avgHue = (Math.atan2(hueY / count, hueX / count) / (2 * Math.PI) + 1) % 1;
          // Apply fixed saturation and lightness
          resolve(hslToRgb(avgHue, saturation, lightness));
        } else {
          resolve(null);
        }
      } catch {
        // CORS or other error
        resolve(null);
      }
    };

    img.onerror = () => resolve(null);
    img.src = url;
  });
}

/**
 * Convert RGB to CSS rgba string
 */
export function rgbToRgba(color: RGB, alpha: number): string {
  return `rgba(${color.r}, ${color.g}, ${color.b}, ${alpha})`;
}
