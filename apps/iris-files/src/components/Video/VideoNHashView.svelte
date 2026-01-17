<script lang="ts">
  /**
   * VideoNHashView - Video player for content-addressed permalinks
   * The nhash points to the video file content; directory metadata is optional.
   */
  import { untrack } from 'svelte';
  import { nhashDecode, type CID } from 'hashtree';
  import { getTree } from '../../store';
  import ShareButton from '../ShareButton.svelte';
  import { getNhashFileUrl } from '../../lib/mediaUrl';
  import VideoComments from './VideoComments.svelte';
  import VideoDescription from './VideoDescription.svelte';
  import VideoLayout from './VideoLayout.svelte';

  interface Props {
    nhash: string;
  }

  let { nhash }: Props = $props();

  let videoFileName = $state<string>('video.mp4');
  let error = $state<string | null>(null);
  let videoCid = $state<CID | null>(null);
  let videoRef: HTMLVideoElement | undefined = $state();

  // Metadata
  let videoTitle = $state<string>('');
  let videoDescription = $state<string>('');

  let normalizedNhash = $derived.by(() => {
    if (typeof nhash !== 'string') return '';
    return nhash.startsWith('hashtree:') ? nhash.slice(9) : nhash;
  });

  // Decode nhash to CID
  let decodedCid = $derived.by(() => {
    if (!normalizedNhash) return null;
    try {
      return nhashDecode(normalizedNhash);
    } catch (e) {
      console.error('[VideoNHashView] Failed to decode nhash:', normalizedNhash, e);
      return null;
    }
  });

  let nhashError = $derived.by(() => {
    if (!normalizedNhash) return 'Invalid nhash format';
    if (!decodedCid) return 'Invalid nhash format';
    return null;
  });

  let activeVideoCid = $derived.by(() => videoCid ?? decodedCid);

  let videoSrc = $derived.by(() => {
    if (!activeVideoCid) return '';
    return getNhashFileUrl(activeVideoCid, videoFileName || 'video.mp4');
  });

  let displayError = $derived.by(() => nhashError || error);
  let loading = $derived.by(() => !displayError && !videoSrc);

  // Load video when nhash changes
  $effect(() => {
    error = null;
    videoTitle = '';
    videoDescription = '';
    videoFileName = 'video.mp4';

    const cid = decodedCid;
    if (cid) {
      videoCid = cid;
      untrack(() => loadVideoDirectory(cid));
    } else {
      videoCid = null;
    }
  });

  async function loadVideoDirectory(cidParam: CID) {
    try {
      const tree = getTree();
      const entries = await tree.listDirectory(cidParam);

      if (!entries || entries.length === 0) {
        return;
      }

      const videoEntry = entries.find(e =>
        e.name.startsWith('video.') ||
        e.name.endsWith('.webm') ||
        e.name.endsWith('.mp4') ||
        e.name.endsWith('.mov') ||
        e.name.endsWith('.mkv')
      );

      if (!videoEntry) {
        return;
      }

      videoCid = videoEntry.cid;
      videoFileName = videoEntry.name;

      if (videoEntry.meta) {
        if (typeof videoEntry.meta.title === 'string') videoTitle = videoEntry.meta.title;
        if (typeof videoEntry.meta.description === 'string') videoDescription = videoEntry.meta.description;
      }

      if (!videoTitle) {
        try {
          const titleResult = await tree.resolvePath(cidParam, 'title.txt');
          if (titleResult) {
            const titleData = await tree.readFile(titleResult.cid);
            if (titleData) videoTitle = new TextDecoder().decode(titleData).trim();
          }
        } catch {}
      }

      if (!videoDescription) {
        try {
          const descResult = await tree.resolvePath(cidParam, 'description.txt');
          if (descResult) {
            const descData = await tree.readFile(descResult.cid);
            if (descData) videoDescription = new TextDecoder().decode(descData).trim();
          }
        } catch {}
      }

    } catch (e) {
      console.error('[VideoNHashView] Failed to load directory:', e);
    }
  }

  function handleDownload() {
    if (!activeVideoCid) return;
    window.location.href = getNhashFileUrl(activeVideoCid, videoFileName || 'video.mp4') + '?download=1';
  }
</script>

{#snippet videoPlayer()}
  {#if loading}
    <div class="w-full h-full flex items-center justify-center bg-black text-white" data-testid="video-loading">
      <span class="i-lucide-loader-2 text-4xl text-text-3 animate-spin"></span>
    </div>
  {:else if displayError}
    <div class="w-full h-full flex items-center justify-center bg-black text-red-400" data-testid="video-error">
      <span class="i-lucide-alert-circle mr-2"></span>
      {displayError}
    </div>
  {:else}
    <!-- svelte-ignore a11y_media_has_caption -->
    <video
      bind:this={videoRef}
      src={videoSrc}
      class="w-full h-full bg-black"
      controls
      autoplay
      playsinline
      data-testid="video-player"
    ></video>
  {/if}
{/snippet}

{#snippet videoContent()}
  {#if videoTitle}
    <h1 class="text-xl font-semibold text-text-1 mb-4" data-testid="video-title">{videoTitle}</h1>
  {/if}

  <div class="flex items-center gap-2 flex-wrap mb-4">
    <ShareButton url={window.location.href} />
    <button onclick={handleDownload} class="btn-ghost" disabled={!videoCid} title="Download">
      <span class="i-lucide-download text-base"></span>
      <span class="hidden sm:inline ml-1">Download</span>
    </button>
  </div>

  {#if videoDescription}
    <VideoDescription description={videoDescription} />
  {/if}

  <div class="bg-surface-1 rounded-lg p-3 text-sm text-text-3 my-4">
    <p>This is a content-addressed permalink. The video is identified by its content hash, not by any user or channel.</p>
  </div>

  <VideoComments {nhash} filename={videoFileName || 'video.mp4'} />
{/snippet}

<VideoLayout {videoPlayer} {videoContent} currentHref={`#/${nhash}`} />
