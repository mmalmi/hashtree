import { writable, get } from 'svelte/store';
import { cid } from 'hashtree';
import type { StreamWriter, CID } from 'hashtree';
import { getTree } from '../../store';
import { autosaveIfOwn, nostrStore } from '../../nostr';
import { parseRoute } from '../../utils/route';
import { getCurrentPathFromUrl } from '../../actions/route';
import { getTreeRootSync } from '../../stores/treeRoot';
import { markFilesChanged } from '../../stores/recentlyChanged';
import { patchWebmDuration } from '../../utils/webmDuration';
import { BoundedQueue } from '../../utils/boundedQueue';

// Generate default stream filename
export function getDefaultFilename(): string {
  const d = new Date();
  const pad = (n: number) => n.toString().padStart(2, '0');
  return `stream_${d.getFullYear()}_${pad(d.getMonth() + 1)}_${pad(d.getDate())}_${pad(d.getHours())}${pad(d.getMinutes())}`;
}

// Stream state interface
interface StreamState {
  isRecording: boolean;
  isPreviewing: boolean;
  recordingTime: number;
  streamFilename: string;
  persistStream: boolean;
  streamWriter: StreamWriter | null;
  streamStats: { chunks: number; buffered: number; totalSize: number };
}

// Initial state
const initialState: StreamState = {
  isRecording: false,
  isPreviewing: false,
  recordingTime: 0,
  streamFilename: getDefaultFilename(),
  persistStream: true,
  streamWriter: null,
  streamStats: { chunks: 0, buffered: 0, totalSize: 0 },
};

// Create Svelte store
export const streamStore = writable<StreamState>(initialState);

// Non-hook getter for use in non-reactive code
export function getStreamState(): StreamState {
  return get(streamStore);
}

// State setters
export function setIsRecording(recording: boolean) {
  streamStore.update(s => ({ ...s, isRecording: recording }));
}

export function setIsPreviewing(previewing: boolean) {
  streamStore.update(s => ({ ...s, isPreviewing: previewing }));
}

export function setRecordingTime(time: number) {
  streamStore.update(s => ({ ...s, recordingTime: time }));
}

export function setStreamFilename(filename: string) {
  streamStore.update(s => ({ ...s, streamFilename: filename }));
}

export function setPersistStream(persist: boolean) {
  streamStore.update(s => ({ ...s, persistStream: persist }));
}

export function setStreamWriter(streamWriter: StreamWriter | null) {
  streamStore.update(s => ({ ...s, streamWriter }));
}

export function setStreamStats(stats: { chunks: number; buffered: number; totalSize: number }) {
  streamStore.update(s => ({ ...s, streamStats: stats }));
}

// Module state for media
let mediaStream: MediaStream | null = null;
let mediaRecorder: MediaRecorder | null = null;
let recordingInterval: number | null = null;
let publishInterval: number | null = null;
// Bounded queue for non-persist mode: 30 chunks max, ~4MB max (1Mbps * 30s)
const recentChunks = new BoundedQueue<Uint8Array>({
  maxItems: 30,
  maxBytes: 4 * 1024 * 1024,
  getBytes: (chunk) => chunk.byteLength,
});

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

function concatChunks(chunks: Uint8Array[]): Uint8Array {
  const totalLen = chunks.reduce((sum, c) => sum + c.length, 0);
  const result = new Uint8Array(totalLen);
  let offset = 0;
  for (const chunk of chunks) {
    result.set(chunk, offset);
    offset += chunk.length;
  }
  return result;
}

export async function startRecording(videoEl: HTMLVideoElement | null): Promise<void> {
  if (!mediaStream) {
    await startPreview(videoEl);
    if (!mediaStream) return;
  }

  // Reset state
  recentChunks.clear();
  const tree = getTree();
  const newStreamWriter = tree.createStream();
  setStreamWriter(newStreamWriter);
  setStreamStats({ chunks: 0, buffered: 0, totalSize: 0 });

  mediaRecorder = new MediaRecorder(mediaStream, {
    mimeType: 'video/webm;codecs=vp8,opus',
    videoBitsPerSecond: 1000000,
  });

  mediaRecorder.ondataavailable = async (event) => {
    if (event.data.size > 0) {
      const chunk = new Uint8Array(await event.data.arrayBuffer());
      const currentState = getStreamState();

      if (currentState.persistStream) {
        const streamWriter = currentState.streamWriter;
        if (streamWriter) {
          await streamWriter.append(chunk);
          setStreamStats(streamWriter.stats);
        }
      } else {
        recentChunks.push(chunk);
        setStreamStats({
          chunks: recentChunks.length,
          buffered: 0,
          totalSize: recentChunks.bytes,
        });
      }
    }
  };

  mediaRecorder.start(1000);
  setIsRecording(true);
  setRecordingTime(0);

  recordingInterval = window.setInterval(() => {
    const currentState = getStreamState();
    setRecordingTime(currentState.recordingTime + 1);
  }, 1000);

  // Publish to nostr every 3 seconds (check login/tree state inside interval)
  publishInterval = window.setInterval(async () => {
    const nostrState = nostrStore.getState();
    // Only publish if logged in and have a selected tree
    if (!nostrState.isLoggedIn || !nostrState.selectedTree) {
      return;
    }

    const currentState = getStreamState();
    const route = parseRoute();
    const rootCid = getTreeRootSync(route.npub, route.treeName);
    const filename = `${currentState.streamFilename}.webm`;
    const durationMs = currentState.recordingTime * 1000; // Current duration in ms

    const tree = getTree();
    let fileCid: CID | undefined, fileSize: number | undefined;
    if (currentState.persistStream && currentState.streamWriter) {
      const result = await currentState.streamWriter.finalize();
      // StreamWriter returns { hash, size, key? } - use key for encrypted CID
      fileCid = cid(result.hash, result.key);
      fileSize = result.size;
    } else if (!currentState.persistStream && !recentChunks.isEmpty) {
      const combined = concatChunks(recentChunks.toArray());
      // Always encrypt files (CHK encryption for deduplication)
      const result = await tree.putFile(combined);
      fileCid = result.cid;
      fileSize = result.size;
    } else {
      return;
    }

    // Patch WebM duration so viewers can see current duration
    if (fileCid && durationMs > 0) {
      fileCid = await patchWebmDuration(tree, fileCid, durationMs);
    }

    if (rootCid) {
      const currentPath = getCurrentPathFromUrl();
      const newRootCid = await tree.setEntry(rootCid, currentPath, filename, fileCid!, fileSize);
      // Publish to nostr - resolver will pick up the update
      autosaveIfOwn(newRootCid);
      markFilesChanged(new Set([filename]));
    } else {
      // Create new tree - public directory but with encrypted file entries
      const newRootCid = (await tree.putDirectory([{ name: filename, cid: fileCid!, size: fileSize }], {})).cid;
      autosaveIfOwn(newRootCid);
      markFilesChanged(new Set([filename]));
    }
  }, 3000);
}

export async function stopRecording(): Promise<void> {
  if (recordingInterval) {
    clearInterval(recordingInterval);
    recordingInterval = null;
  }

  if (publishInterval) {
    clearInterval(publishInterval);
    publishInterval = null;
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

  const currentState = getStreamState();
  const filename = `${currentState.streamFilename}.webm`;
  const durationMs = currentState.recordingTime * 1000; // Convert seconds to ms

  const tree = getTree();
  let fileCid: CID | undefined, fileSize: number | undefined;
  if (currentState.persistStream && currentState.streamWriter) {
    const result = await currentState.streamWriter.finalize();
    // StreamWriter returns { hash, size, key? } - use key for encrypted CID
    fileCid = cid(result.hash, result.key);
    fileSize = result.size;
  } else if (!currentState.persistStream && !recentChunks.isEmpty) {
    const combined = concatChunks(recentChunks.toArray());
    // Always encrypt files (CHK encryption for deduplication)
    const result = await tree.putFile(combined);
    fileCid = result.cid;
    fileSize = result.size;
  }

  // Patch WebM duration in the file header
  if (fileCid && durationMs > 0) {
    console.log(`[Stream] Patching WebM duration: ${durationMs}ms`);
    fileCid = await patchWebmDuration(tree, fileCid, durationMs);
  }

  if (fileCid && fileSize) {
    const route = parseRoute();
    const rootCid = getTreeRootSync(route.npub, route.treeName);
    if (rootCid) {
      const currentPath = getCurrentPathFromUrl();
      const newRootCid = await tree.setEntry(rootCid, currentPath, filename, fileCid, fileSize);
      // Publish to nostr - resolver will pick up the update
      autosaveIfOwn(newRootCid);
    } else {
      // Create new tree - public directory but with encrypted file entries
      const newRootCid = (await tree.putDirectory([{ name: filename, cid: fileCid, size: fileSize }], {})).cid;
      autosaveIfOwn(newRootCid);
      window.location.hash = '#/';
    }
  }

  setStreamWriter(null);
  recentChunks.clear();

  // Close streaming mode by removing ?stream=1 and ?live=1 from URL
  const hash = window.location.hash;
  if (hash.includes('stream=1') || hash.includes('live=1')) {
    const newHash = hash
      .replace(/[?&]stream=1/g, '')
      .replace(/[?&]live=1/g, '')
      .replace(/\?$/, '') // Remove trailing ? if no other params
      .replace(/\?&/, '?'); // Fix ?& to just ?
    window.location.hash = newHash;
  }
}

export function formatTime(seconds: number): string {
  const m = Math.floor(seconds / 60);
  const s = seconds % 60;
  return `${m}:${s.toString().padStart(2, '0')}`;
}
