/**
 * Video transcoding utility using FFmpeg WASM
 * Lazy-loads FFmpeg only when needed (for non-webm/mp4 files)
 */

import type { FFmpeg } from '@ffmpeg/ffmpeg';
import { getHtreePrefix } from '../lib/mediaUrl';

let ffmpegInstance: FFmpeg | null = null;
let loadingPromise: Promise<FFmpeg> | null = null;

/**
 * Check if transcoding is supported (requires SharedArrayBuffer)
 */
export function isTranscodingSupported(): boolean {
  return typeof SharedArrayBuffer !== 'undefined';
}

/**
 * Check if a file can be transcoded
 */
export function canTranscode(_file: File): { ok: boolean; reason?: string } {
  if (!isTranscodingSupported()) {
    return { ok: false, reason: 'SharedArrayBuffer not available (requires cross-origin isolation)' };
  }
  return { ok: true };
}

/**
 * Formats that browsers can generally play natively
 * Note: Even mp4 can contain unsupported codecs (HEVC on some browsers)
 */
const BROWSER_PLAYABLE_EXTENSIONS = new Set(['mp4', 'webm', 'm4v']);

/**
 * Formats that definitely need transcoding
 */
const NEEDS_TRANSCODING_EXTENSIONS = new Set([
  'mov',    // QuickTime - often contains HEVC or ProRes
  'mkv',    // Matroska - no browser support
  'avi',    // AVI - no browser support
  'wmv',    // Windows Media - no browser support
  'flv',    // Flash Video - no browser support
  'ogv',    // Ogg Video - limited support
  '3gp',    // 3GPP - limited support
  'ts',     // MPEG Transport Stream
  'mts',    // AVCHD
  'm2ts',   // Blu-ray
  'vob',    // DVD
  'divx',   // DivX
  'xvid',   // XviD
  'asf',    // Advanced Systems Format
  'rm',     // RealMedia
  'rmvb',   // RealMedia Variable Bitrate
]);

/**
 * Check if a file needs transcoding based on extension
 * Returns true for formats browsers can't play natively
 */
export function needsTranscoding(file: File): boolean {
  const ext = file.name.split('.').pop()?.toLowerCase();

  // No extension or unknown - needs transcoding to be safe
  if (!ext) return true;

  // Known problematic formats - definitely need transcoding
  if (NEEDS_TRANSCODING_EXTENSIONS.has(ext)) return true;

  // Known playable formats - no transcoding needed
  if (BROWSER_PLAYABLE_EXTENSIONS.has(ext)) return false;

  // Unknown extension - assume needs transcoding
  return true;
}

/**
 * Get human-readable info about why a format might not play
 */
export function getFormatInfo(fileName: string): { playable: boolean; reason?: string } {
  const ext = fileName.split('.').pop()?.toLowerCase();

  if (!ext) {
    return { playable: false, reason: 'Unknown format (no file extension)' };
  }

  if (NEEDS_TRANSCODING_EXTENSIONS.has(ext)) {
    const formatNames: Record<string, string> = {
      mov: 'QuickTime',
      mkv: 'Matroska',
      avi: 'AVI',
      wmv: 'Windows Media',
      flv: 'Flash Video',
    };
    return {
      playable: false,
      reason: `${formatNames[ext] || ext.toUpperCase()} format is not supported by browsers`
    };
  }

  if (BROWSER_PLAYABLE_EXTENSIONS.has(ext)) {
    return { playable: true };
  }

  return { playable: false, reason: `Unknown format (.${ext})` };
}

/**
 * Codecs that browsers generally cannot play
 */
const UNSUPPORTED_CODECS = new Set([
  'hevc', 'h265',     // HEVC/H.265 - limited Safari support, no Chrome/Firefox
  'vp9',              // VP9 in mp4 - Safari doesn't support
  'av1',              // AV1 - limited support
  'prores',           // ProRes - Apple professional codec
  'dnxhd', 'dnxhr',   // Avid DNx codecs
  'mjpeg',            // Motion JPEG
  'mpeg2video',       // MPEG-2
  'mpeg4',            // MPEG-4 Part 2 (DivX/XviD)
  'msmpeg4v3',        // MS MPEG-4
  'wmv1', 'wmv2', 'wmv3', // Windows Media Video
  'rv10', 'rv20', 'rv30', 'rv40', // RealVideo
]);

/**
 * Probe video file to detect codec
 * Returns codec info without full transcoding
 */
export async function probeVideoCodec(file: File): Promise<{
  videoCodec?: string;
  audioCodec?: string;
  needsTranscoding: boolean;
  reason?: string;
}> {
  // Quick extension check first
  const ext = file.name.split('.').pop()?.toLowerCase();
  if (ext && NEEDS_TRANSCODING_EXTENSIONS.has(ext)) {
    return {
      needsTranscoding: true,
      reason: `${ext.toUpperCase()} format needs transcoding`
    };
  }

  // For mp4/webm, we could probe codec but it requires loading FFmpeg
  // For now, return based on extension - codec probe is expensive
  if (ext && BROWSER_PLAYABLE_EXTENSIONS.has(ext)) {
    return { needsTranscoding: false };
  }

  return {
    needsTranscoding: true,
    reason: 'Unknown format'
  };
}

/**
 * Check if a video codec is browser-playable
 */
export function isCodecSupported(codec: string): boolean {
  const normalized = codec.toLowerCase();
  if (UNSUPPORTED_CODECS.has(normalized)) return false;

  // Known supported codecs
  const supported = ['h264', 'avc1', 'vp8', 'aac', 'mp3', 'opus', 'vorbis'];
  return supported.some(s => normalized.includes(s));
}

// FFmpeg core files hosted on hashtree (content-addressed, immutable)
// Service worker (web) or native protocol handler (Tauri) handles htree paths
const FFMPEG_NHASH = 'nhash1qqs0297vyhmzhu6nq6xuynxwtrfgsqrrttll0utaeykat7gxrtkf2hg9ypelsumyf9d09ndhnd9de5usvzp50y6sfq0cpm8fu2997g884lfm562m4rw';

/**
 * Lazy load FFmpeg WASM from hashtree via service worker (web) or native handler (Tauri)
 * Content-addressed storage ensures integrity and enables caching
 */
async function loadFFmpeg(): Promise<FFmpeg> {
  if (ffmpegInstance) return ffmpegInstance;
  if (loadingPromise) return loadingPromise;

  loadingPromise = (async () => {
    const { FFmpeg } = await import('@ffmpeg/ffmpeg');

    const ffmpeg = new FFmpeg();
    const prefix = getHtreePrefix();

    // Load via service worker (web) or native HTTP server (Tauri)
    await ffmpeg.load({
      coreURL: `${prefix}/htree/${FFMPEG_NHASH}/ffmpeg-core.js`,
      wasmURL: `${prefix}/htree/${FFMPEG_NHASH}/ffmpeg-core.wasm`,
    });
    ffmpegInstance = ffmpeg;
    return ffmpeg;
  })();

  return loadingPromise;
}

export interface TranscodeProgress {
  stage: 'loading' | 'transcoding' | 'done';
  message: string;
  percent?: number;
}

/**
 * Transcode video to MP4 format with streaming output
 * Outputs chunks to onChunk callback after transcoding completes
 *
 * Note: FFmpeg WASM runs synchronously, so we can't stream during encoding.
 * But we can stream the output to hashtree in chunks after encoding finishes.
 *
 * @param onChunk - Called with output chunks after transcoding
 * @param signal - Optional AbortSignal to cancel transcoding
 */
export async function transcodeToMP4Streaming(
  file: File,
  onChunk: (chunk: Uint8Array) => Promise<void>,
  onProgress?: (progress: TranscodeProgress) => void,
  signal?: AbortSignal
): Promise<{ mimeType: string; extension: string }> {
  const check = canTranscode(file);
  if (!check.ok) {
    throw new Error(check.reason);
  }

  if (signal?.aborted) {
    throw new Error('Cancelled');
  }

  onProgress?.({ stage: 'loading', message: 'Loading video encoder...' });

  let ffmpeg;
  try {
    ffmpeg = await loadFFmpeg();
  } catch (e) {
    throw new Error(`Failed to load video encoder: ${e instanceof Error ? e.message : String(e)}`);
  }

  if (signal?.aborted) {
    throw new Error('Cancelled');
  }

  const inputName = 'input' + getExtension(file.name);
  const outputName = 'output.mp4';

  onProgress?.({ stage: 'transcoding', message: 'Preparing video...', percent: 0 });

  // Write input file
  try {
    const { fetchFile } = await import('@ffmpeg/util');
    await ffmpeg.writeFile(inputName, await fetchFile(file));
  } catch (e) {
    throw new Error(`Failed to read video file: ${e instanceof Error ? e.message : String(e)}`);
  }

  if (signal?.aborted) {
    try { await ffmpeg.deleteFile(inputName); } catch {}
    throw new Error('Cancelled');
  }

  onProgress?.({ stage: 'transcoding', message: 'Starting transcode...', percent: 5 });

  // Set up progress handler
  ffmpeg.on('progress', ({ progress }: { progress: number }) => {
    // 5-85% for transcoding
    const percent = 5 + Math.round(progress * 80);
    onProgress?.({
      stage: 'transcoding',
      message: `Transcoding: ${Math.round(progress * 100)}%`,
      percent
    });
  });

  // Set up abort handler - terminate FFmpeg on cancel
  const abortHandler = () => {
    ffmpeg.terminate();
    // Reset instance so next transcode creates fresh one
    ffmpegInstance = null;
    loadingPromise = null;
  };
  signal?.addEventListener('abort', abortHandler);

  // Transcode to MP4 with H.264
  try {
    await ffmpeg.exec([
      '-i', inputName,
      '-c:v', 'libx264',
      '-preset', 'veryfast',     // Fast encoding
      '-crf', '23',              // Good quality
      '-c:a', 'aac',
      '-b:a', '128k',
      '-vf', 'scale=-2:720',     // 720p max
      '-movflags', '+faststart', // Web-optimized
      outputName
    ]);
  } catch (e) {
    signal?.removeEventListener('abort', abortHandler);
    try { await ffmpeg.deleteFile(inputName); } catch {}
    if (signal?.aborted) {
      throw new Error('Cancelled');
    }
    throw new Error(`Transcoding failed: ${e instanceof Error ? e.message : String(e)}`);
  }

  signal?.removeEventListener('abort', abortHandler);

  if (signal?.aborted) {
    try { await ffmpeg.deleteFile(inputName); } catch {}
    try { await ffmpeg.deleteFile(outputName); } catch {}
    throw new Error('Cancelled');
  }

  // Delete input immediately to free memory before reading output
  try { await ffmpeg.deleteFile(inputName); } catch {}

  onProgress?.({ stage: 'transcoding', message: 'Saving video...', percent: 85 });

  // Read output and stream in chunks
  try {
    const outputData = await ffmpeg.readFile(outputName) as Uint8Array;
    await ffmpeg.deleteFile(outputName); // Free output memory in WASM

    // Stream output in 1MB chunks
    const chunkSize = 1024 * 1024;
    for (let i = 0; i < outputData.length; i += chunkSize) {
      if (signal?.aborted) {
        throw new Error('Cancelled');
      }
      const chunk = outputData.slice(i, Math.min(i + chunkSize, outputData.length));
      await onChunk(chunk);

      const savePercent = 85 + Math.round((i / outputData.length) * 15);
      onProgress?.({
        stage: 'transcoding',
        message: `Saving: ${Math.round(i / 1024 / 1024)}MB / ${Math.round(outputData.length / 1024 / 1024)}MB`,
        percent: savePercent
      });
    }
  } catch (e) {
    if (signal?.aborted) {
      throw new Error('Cancelled');
    }
    throw new Error(`Failed to read transcoded video: ${e instanceof Error ? e.message : String(e)}`);
  }

  onProgress?.({ stage: 'done', message: 'Transcoding complete', percent: 100 });

  return {
    mimeType: 'video/mp4',
    extension: 'mp4'
  };
}

function getExtension(filename: string): string {
  const ext = filename.split('.').pop()?.toLowerCase();
  return ext ? `.${ext}` : '';
}
