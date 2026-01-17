/**
 * Video Streaming State for Video App
 *
 * Handles webcam/mic recording and saving to videos/{title} trees.
 * Publishes tree updates every 3 seconds for live streaming.
 */

import { writable, get } from 'svelte/store';
import { cid, fixedChunker } from 'hashtree';
import type { StreamWriter, CID, TreeVisibility } from 'hashtree';
import { getTree } from '../../store';
import { saveHashtree } from '../../nostr';
import { nostrStore } from '../../nostr';
import { addRecent } from '../../stores/recents';
import { storeLinkKey } from '../../stores/trees';
import { patchWebmDuration } from '../../utils/webmDuration';

// Chunk size for live streaming: 128KB for more frequent updates
// Smaller chunks help with live streaming latency
const STREAM_CHUNK_SIZE = 128 * 1024;

// Stream state interface
interface VideoStreamState {
  isRecording: boolean;
  isPreviewing: boolean;
  recordingTime: number;
  streamWriter: StreamWriter | null;
  streamStats: { chunks: number; buffered: number; totalSize: number };
}

// Initial state
const initialState: VideoStreamState = {
  isRecording: false,
  isPreviewing: false,
  recordingTime: 0,
  streamWriter: null,
  streamStats: { chunks: 0, buffered: 0, totalSize: 0 },
};

// Create Svelte store
export const videoStreamStore = writable<VideoStreamState>(initialState);

// Non-hook getter for use in non-reactive code
export function getVideoStreamState(): VideoStreamState {
  return get(videoStreamStore);
}

// State setters
export function setIsRecording(recording: boolean) {
  videoStreamStore.update(s => ({ ...s, isRecording: recording }));
}

export function setIsPreviewing(previewing: boolean) {
  videoStreamStore.update(s => ({ ...s, isPreviewing: previewing }));
}

export function setRecordingTime(time: number) {
  videoStreamStore.update(s => ({ ...s, recordingTime: time }));
}

export function setStreamWriter(streamWriter: StreamWriter | null) {
  videoStreamStore.update(s => ({ ...s, streamWriter }));
}

export function setStreamStats(stats: { chunks: number; buffered: number; totalSize: number }) {
  videoStreamStore.update(s => ({ ...s, streamStats: stats }));
}

// Module state for media
let mediaStream: MediaStream | null = null;
let mediaRecorder: MediaRecorder | null = null;
let recordingInterval: number | null = null;
let chunkCheckInterval: number | null = null;

// Track current stream metadata for publishing
let currentStreamTitle: string = '';
let currentStreamVisibility: TreeVisibility = 'public';
let lastPublishedChunkCount: number = 0;
let isPublishing: boolean = false;

export function getMediaStream(): MediaStream | null {
  return mediaStream;
}

export async function startPreview(videoEl: HTMLVideoElement | null): Promise<void> {
  try {
    mediaStream = await navigator.mediaDevices.getUserMedia({
      video: true,
      audio: true,
    });
    if (videoEl) {
      videoEl.srcObject = mediaStream;
    }
    setIsPreviewing(true);
  } catch (e) {
    console.error('Camera error:', e);
    throw e;
  }
}

export function stopPreview(videoEl: HTMLVideoElement | null): void {
  if (mediaStream) {
    mediaStream.getTracks().forEach(track => track.stop());
    mediaStream = null;
  }
  if (videoEl) {
    videoEl.srcObject = null;
  }
  setIsPreviewing(false);
}

export async function startRecording(
  videoEl: HTMLVideoElement | null,
  isPublic: boolean,
  title: string,
  visibility: TreeVisibility
): Promise<void> {
  if (!mediaStream) {
    await startPreview(videoEl);
    if (!mediaStream) return;
  }

  // Store metadata for publishing
  currentStreamTitle = title;
  currentStreamVisibility = visibility;
  lastPublishedChunkCount = 0;
  isPublishing = false;

  // Reset state - use fixed small chunks for live streaming
  const tree = getTree();
  const newStreamWriter = tree.createStream({ chunker: fixedChunker(STREAM_CHUNK_SIZE) });
  setStreamWriter(newStreamWriter);
  setStreamStats({ chunks: 0, buffered: 0, totalSize: 0 });

  mediaRecorder = new MediaRecorder(mediaStream, {
    mimeType: 'video/webm;codecs=vp8,opus',
    videoBitsPerSecond: 1000000,
  });

  mediaRecorder.ondataavailable = async (event) => {
    if (event.data.size > 0) {
      const chunk = new Uint8Array(await event.data.arrayBuffer());
      const currentState = getVideoStreamState();

      const streamWriter = currentState.streamWriter;
      if (streamWriter) {
        await streamWriter.append(chunk);
        setStreamStats(streamWriter.stats);
      }
    }
  };

  mediaRecorder.start(1000); // 1 second chunks from MediaRecorder
  setIsRecording(true);
  setRecordingTime(0);

  recordingInterval = window.setInterval(() => {
    const currentState = getVideoStreamState();
    setRecordingTime(currentState.recordingTime + 1);
  }, 1000);

  // Check for new chunks every 500ms and publish when we have new ones
  chunkCheckInterval = window.setInterval(async () => {
    await checkAndPublish();
  }, 500);
}

/**
 * Check if new chunks are available and publish if so
 * Only publishes when chunk count increases (no duplicate data)
 */
async function checkAndPublish(): Promise<void> {
  // Skip if already publishing or not recording
  if (isPublishing) return;

  const currentState = getVideoStreamState();
  if (!currentState.streamWriter || !currentState.isRecording) return;

  // Only publish when we have new complete chunks
  const currentChunkCount = currentState.streamStats.chunks;
  if (currentChunkCount <= lastPublishedChunkCount) return;

  const nostrState = nostrStore.getState();
  if (!nostrState.isLoggedIn || !nostrState.npub) return;

  isPublishing = true;
  try {
    const tree = getTree();
    const treeName = `videos/${currentStreamTitle.trim()}`;

    // Get current root without finalizing (preserves buffer for continued streaming)
    const fileCid: CID | null = await currentState.streamWriter.currentRoot();
    if (!fileCid) return;
    const fileSize = currentState.streamStats.totalSize;

    // Note: Don't patch duration during live streaming - it would create a different
    // file on each publish. Duration is patched only on final stopRecording().

    // Build video directory with current state
    const entries: Array<{ name: string; cid: CID; size?: number }> = [
      { name: 'video.webm', cid: fileCid, size: fileSize },
    ];

    // Add metadata.json
    const createdAt = Math.floor(Date.now() / 1000);
    const metadata: Record<string, unknown> = {
      createdAt,
      title: currentStreamTitle.trim(),
    };
    const metadataBytes = new TextEncoder().encode(JSON.stringify(metadata, null, 2));
    const metadataResult = await tree.putFile(metadataBytes, {});
    entries.push({ name: 'metadata.json', cid: metadataResult.cid, size: metadataResult.size });

    // Create directory and publish
    const dirResult = await tree.putDirectory(entries, {});

    // Publish to Nostr - viewers subscribed to this tree will get the update
    await saveHashtree(treeName, dirResult.cid, { visibility: currentStreamVisibility });

    lastPublishedChunkCount = currentChunkCount;
  } finally {
    isPublishing = false;
  }
}

interface StopRecordingResult {
  success: boolean;
  videoUrl?: string;
}

export async function stopRecording(
  title: string,
  description: string,
  visibility: TreeVisibility,
  thumbnailBlob: Blob | null
): Promise<StopRecordingResult> {
  if (recordingInterval) {
    clearInterval(recordingInterval);
    recordingInterval = null;
  }

  if (chunkCheckInterval) {
    clearInterval(chunkCheckInterval);
    chunkCheckInterval = null;
  }

  if (mediaRecorder && mediaRecorder.state !== 'inactive') {
    mediaRecorder.stop();
    mediaRecorder = null;
  }

  // Stop media stream (camera/microphone)
  if (mediaStream) {
    mediaStream.getTracks().forEach(track => track.stop());
    mediaStream = null;
  }

  setIsRecording(false);
  setIsPreviewing(false);

  const currentState = getVideoStreamState();
  const durationMs = currentState.recordingTime * 1000;

  const tree = getTree();
  let fileCid: CID | undefined;
  let fileSize: number | undefined;

  if (currentState.streamWriter) {
    const result = await currentState.streamWriter.finalize();
    fileCid = cid(result.hash, result.key);
    fileSize = result.size;
  }

  // Patch WebM duration
  if (fileCid && durationMs > 0) {
    console.log(`[VideoStream] Patching WebM duration: ${durationMs}ms`);
    fileCid = await patchWebmDuration(tree, fileCid, durationMs);
  }

  if (!fileCid || !fileSize) {
    setStreamWriter(null);
    return { success: false };
  }

  const nostrState = nostrStore.getState();
  const userNpub = nostrState.npub;

  if (!userNpub) {
    setStreamWriter(null);
    return { success: false };
  }

  // Build video directory
  const treeName = `videos/${title.trim()}`;
  const entries: Array<{ name: string; cid: CID; size?: number }> = [
    { name: 'video.webm', cid: fileCid, size: fileSize },
  ];

  // Create metadata.json with title, description, and timestamp
  const createdAt = Math.floor(Date.now() / 1000);
  const metadata: Record<string, unknown> = {
    createdAt,
    title: title.trim(),
  };
  if (description.trim()) metadata.description = description.trim();

  const metadataBytes = new TextEncoder().encode(JSON.stringify(metadata, null, 2));
  const metadataResult = await tree.putFile(metadataBytes, {});
  entries.push({ name: 'metadata.json', cid: metadataResult.cid, size: metadataResult.size });

  // Upload thumbnail if available
  if (thumbnailBlob) {
    const thumbData = new Uint8Array(await thumbnailBlob.arrayBuffer());
    const thumbResult = await tree.putFile(thumbData, {});
    entries.push({ name: 'thumbnail.jpg', cid: thumbResult.cid, size: thumbResult.size });
  }

  // Create directory
  const dirResult = await tree.putDirectory(entries, {});

  // Publish to Nostr
  const result = await saveHashtree(treeName, dirResult.cid, { visibility });

  // Store link key for link-visible videos
  if (result.linkKey && userNpub) {
    storeLinkKey(userNpub, treeName, result.linkKey);
  }

  // Add to recents
  addRecent({
    type: 'tree',
    path: `/${userNpub}/${treeName}`,
    label: title.trim(),
    npub: userNpub,
    treeName,
    visibility,
    linkKey: result.linkKey,
  });

  setStreamWriter(null);

  // Build video URL
  const encodedTreeName = encodeURIComponent(treeName);
  const videoUrl = result.linkKey
    ? `#/${userNpub}/${encodedTreeName}?k=${result.linkKey}`
    : `#/${userNpub}/${encodedTreeName}`;

  return { success: true, videoUrl };
}

export function cancelRecording(): void {
  if (recordingInterval) {
    clearInterval(recordingInterval);
    recordingInterval = null;
  }

  if (chunkCheckInterval) {
    clearInterval(chunkCheckInterval);
    chunkCheckInterval = null;
  }

  if (mediaRecorder && mediaRecorder.state !== 'inactive') {
    mediaRecorder.stop();
    mediaRecorder = null;
  }

  if (mediaStream) {
    mediaStream.getTracks().forEach(track => track.stop());
    mediaStream = null;
  }

  setIsRecording(false);
  setIsPreviewing(false);
  setStreamWriter(null);
  setStreamStats({ chunks: 0, buffered: 0, totalSize: 0 });
  setRecordingTime(0);
  currentStreamTitle = '';
  currentStreamVisibility = 'public';
  lastPublishedChunkCount = 0;
  isPublishing = false;
}

export function formatTime(seconds: number): string {
  const m = Math.floor(seconds / 60);
  const s = seconds % 60;
  return `${m}:${s.toString().padStart(2, '0')}`;
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}
