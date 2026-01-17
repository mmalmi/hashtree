/**
 * yt-dlp directory detection and file grouping utilities
 *
 * yt-dlp downloads files with pattern: "Title [VIDEO_ID].ext"
 * Each video typically has:
 * - Video file: .mp4, .mkv, .webm, etc.
 * - Metadata: .info.json
 * - Thumbnail: .jpg, .webp, .png
 */

// Video extensions that yt-dlp commonly downloads
const VIDEO_EXTENSIONS = new Set([
  '.mp4', '.mkv', '.webm', '.mov', '.avi', '.m4v', '.flv', '.wmv', '.3gp'
]);

// Thumbnail extensions
const THUMBNAIL_EXTENSIONS = new Set(['.jpg', '.jpeg', '.png', '.webp']);

// Regex to extract video ID from yt-dlp filename: "Title [VIDEO_ID].ext"
// YouTube video IDs are exactly 11 characters: a-z, A-Z, 0-9, _, -
// Must be followed by ]. to avoid matching other brackets like [Official Video]
const VIDEO_ID_REGEX = /\[([a-zA-Z0-9_-]{11})\]\./;

export interface YtDlpVideo {
  id: string;
  title: string;
  videoFile: File | null;
  infoJson: File | null;
  thumbnail: File | null;
}

export interface YtDlpDirectoryResult {
  isYtDlpDirectory: boolean;
  videos: YtDlpVideo[];
  channelName: string | null;
  otherFiles: File[];
}

/**
 * Extract YouTube video ID from a yt-dlp filename
 */
export function extractVideoId(filename: string): string | null {
  const match = filename.match(VIDEO_ID_REGEX);
  return match ? match[1] : null;
}

/**
 * Extract title from yt-dlp filename (everything before [VIDEO_ID])
 */
export function extractTitle(filename: string): string {
  // First check if there's a valid video ID pattern
  const match = filename.match(VIDEO_ID_REGEX);
  if (match) {
    // Find the position of the matched video ID bracket
    const bracketPos = filename.indexOf(` [${match[1]}]`);
    if (bracketPos !== -1) {
      return filename.slice(0, bracketPos).trim();
    }
  }
  // No video ID pattern, use filename without extension
  const dotIndex = filename.lastIndexOf('.');
  return dotIndex !== -1 ? filename.slice(0, dotIndex) : filename;
}

/**
 * Get file extension (lowercase, including dot)
 * Handles compound extensions like .info.json
 */
function getExtension(filename: string): string {
  // Check for .info.json first (compound extension)
  if (filename.toLowerCase().endsWith('.info.json')) {
    return '.info.json';
  }
  const dotIndex = filename.lastIndexOf('.');
  return dotIndex !== -1 ? filename.slice(dotIndex).toLowerCase() : '';
}

/**
 * Detect if a directory contains yt-dlp downloaded files and group them by video
 */
export function detectYtDlpDirectory(files: File[]): YtDlpDirectoryResult {
  const videoMap = new Map<string, YtDlpVideo>();
  const otherFiles: File[] = [];
  let channelName: string | null = null;

  for (const file of files) {
    const filename = file.name;
    const videoId = extractVideoId(filename);
    const ext = getExtension(filename);

    if (!videoId) {
      // Check if it's a channel thumbnail (has channel ID pattern)
      // e.g., "Channel Name - Videos [UCxxxxxxxxxxxxxxx].jpg"
      const channelMatch = filename.match(/\[(UC[a-zA-Z0-9_-]{22})\]\./);
      if (channelMatch && THUMBNAIL_EXTENSIONS.has(ext)) {
        // Extract channel name from filename
        const bracketIndex = filename.lastIndexOf(' [');
        if (bracketIndex !== -1) {
          channelName = filename.slice(0, bracketIndex).replace(/ - Videos$/, '').trim();
        }
        otherFiles.push(file);
      } else {
        otherFiles.push(file);
      }
      continue;
    }

    // Get or create video entry
    if (!videoMap.has(videoId)) {
      videoMap.set(videoId, {
        id: videoId,
        title: extractTitle(filename),
        videoFile: null,
        infoJson: null,
        thumbnail: null,
      });
    }

    const video = videoMap.get(videoId)!;

    // Categorize file
    if (ext === '.info.json') {
      video.infoJson = file;
    } else if (VIDEO_EXTENSIONS.has(ext)) {
      video.videoFile = file;
    } else if (THUMBNAIL_EXTENSIONS.has(ext)) {
      video.thumbnail = file;
    } else {
      otherFiles.push(file);
    }
  }

  // Filter to only videos that have at least a video file
  const videos = Array.from(videoMap.values()).filter(v => v.videoFile !== null);

  // Sort by title
  videos.sort((a, b) => a.title.localeCompare(b.title));

  // Consider it a yt-dlp directory if we found at least one video with info.json
  const hasInfoJson = videos.some(v => v.infoJson !== null);
  const isYtDlpDirectory = videos.length > 0 && hasInfoJson;

  return {
    isYtDlpDirectory,
    videos,
    channelName,
    otherFiles,
  };
}

/**
 * Parse yt-dlp info.json to extract metadata
 */
export interface YtDlpMetadata {
  id: string;
  title: string;
  description: string;
  uploader: string;
  uploader_id: string;
  upload_date: string; // YYYYMMDD
  duration: number; // seconds
  view_count?: number;
  like_count?: number;
  channel?: string;
  channel_id?: string;
  webpage_url?: string;
  thumbnail?: string;
}

export async function parseInfoJson(file: File): Promise<YtDlpMetadata | null> {
  try {
    const text = await file.text();
    const data = JSON.parse(text);
    return {
      id: data.id || '',
      title: data.title || '',
      description: data.description || '',
      uploader: data.uploader || data.channel || '',
      uploader_id: data.uploader_id || data.channel_id || '',
      upload_date: data.upload_date || '',
      duration: data.duration || 0,
      view_count: data.view_count,
      like_count: data.like_count,
      channel: data.channel,
      channel_id: data.channel_id,
      webpage_url: data.webpage_url,
      thumbnail: data.thumbnail,
    };
  } catch (e) {
    console.error('Failed to parse info.json:', e);
    return null;
  }
}

/**
 * Format upload date from YYYYMMDD to readable format
 */
export function formatUploadDate(dateStr: string): string {
  if (!dateStr || dateStr.length !== 8) return dateStr;
  const year = dateStr.slice(0, 4);
  const month = dateStr.slice(4, 6);
  const day = dateStr.slice(6, 8);
  return `${year}-${month}-${day}`;
}
