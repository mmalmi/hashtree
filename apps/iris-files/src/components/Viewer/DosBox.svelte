<script lang="ts">
  /**
   * DOSBox Viewer - runs .exe files using emulators (DOSBox in browser)
   *
   * Lazy loads emulators package only when an executable is opened.
   * Mounts the parent directory from hashtree as the DOS C: drive.
   */
  import { LinkType, toHex, type CID } from 'hashtree';
  import { SvelteMap } from 'svelte/reactivity';
  import { getTree } from '../../store';
  import { saveBinaryFile } from '../../actions/file';

  // Import WASM files with ?url to get their resolved paths
  import wdosboxWasmUrl from 'emulators/dist/wdosbox.wasm?url';

  // DOSBox emulator instance type
  interface DosInstance {
    exit: () => Promise<void>;
    events: () => DosEvents;
    width: () => number;
    height: () => number;
    soundFrequency: () => number;
    sendKeyEvent: (code: number, pressed: boolean) => void;
    sendMouseRelativeMotion: (x: number, y: number) => void;
    sendMouseButton: (button: number, pressed: boolean) => void;
    fsTree: () => Promise<FsTree>;
    fsReadFile: (path: string) => Promise<Uint8Array>;
  }

  interface DosEvents {
    onFrameSize: (callback: (width: number, height: number) => void) => void;
    onFrame: (callback: (rgb: Uint8Array | null, rgba: Uint8Array | null) => void) => void;
    onSoundPush: (callback: (samples: Float32Array) => void) => void;
    onExit: (callback: () => void) => void;
  }

  // Filesystem node type from js-dos
  interface FsNode {
    name: string;
    nodes?: FsNode[];
  }

  interface FsTree {
    nodes?: FsNode[];
  }

  // Type declaration for the emulators global
  declare global {
    interface Window {
      emulators: {
        pathPrefix: string;
        dosboxDirect: (bundle: Uint8Array) => Promise<DosInstance>;
        dosboxWorker: (bundle: Uint8Array) => Promise<DosInstance>;
      };
    }
  }

  interface Props {
    /** CID of the parent directory (for sibling files) */
    directoryCid: CID;
    /** Name of the executable file */
    exeName: string;
    /** Callback when user exits DOSBox */
    onExit?: () => void;
  }

  let { directoryCid, exeName, onExit }: Props = $props();

  interface DosFS {
    [path: string]: Uint8Array;
  }

  interface CollectedFiles {
    files: DosFS;
    totalSize: number;
    fileCount: number;
  }

  /**
   * Recursively collect all files from a directory tree
   */
  async function collectDirectoryFiles(
    dirCid: CID,
    basePath: string = '',
    onProgress?: (msg: string) => void
  ): Promise<CollectedFiles> {
    const tree = getTree();
    const entries = await tree.listDirectory(dirCid);
    const files: DosFS = {};
    let totalSize = 0;
    let fileCount = 0;

    for (const entry of entries) {
      const fullPath = basePath ? `${basePath}/${entry.name}` : entry.name;

      if (entry.type === LinkType.Dir) {
        // Recursively collect subdirectory
        const subResult = await collectDirectoryFiles(entry.cid, fullPath, onProgress);
        Object.assign(files, subResult.files);
        totalSize += subResult.totalSize;
        fileCount += subResult.fileCount;
      } else {
        // Read file content
        onProgress?.(`Loading ${fullPath}...`);
        const data = await tree.readFile(entry.cid);
        if (data) {
          files[fullPath] = data;
          totalSize += data.length;
          fileCount++;
        }
      }
    }

    return { files, totalSize, fileCount };
  }

  /**
   * Format bytes to human readable string
   */
  function formatBytes(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  }

  // Component state
  let status = $state<'idle' | 'loading' | 'running' | 'error'>('idle');
  let error = $state<string | null>(null);
  let loadingMessage = $state('');
  let collectedFiles = $state<CollectedFiles | null>(null);
  let isFullscreen = $state(false);

  // Refs
  let containerEl: HTMLDivElement | undefined = $state();
  let dosInstanceRef: DosInstance | null = null;
  let shouldStart = $state(false);

  // Toggle fullscreen for the container
  function toggleFullscreen() {
    if (!containerEl) return;

    if (document.fullscreenElement) {
      document.exitFullscreen();
    } else {
      containerEl.requestFullscreen();
    }
  }

  // Listen for fullscreen changes
  $effect(() => {
    const onFullscreenChange = () => {
      isFullscreen = !!document.fullscreenElement;
    };
    document.addEventListener('fullscreenchange', onFullscreenChange);
    return () => document.removeEventListener('fullscreenchange', onFullscreenChange);
  });

  // Trigger start
  function handleStartClick() {
    if (!collectedFiles) {
      console.warn('[DOSBox] No files collected yet');
      return;
    }
    status = 'loading';
    loadingMessage = 'Preparing...';
    shouldStart = true;
  }

  // Actually start DOSBox after the container is rendered
  $effect(() => {
    if (!shouldStart || !containerEl || !collectedFiles) {
      return;
    }

    let cancelled = false;

    async function initDosBox() {
      console.log('[DOSBox] Initializing...', { hasContainer: !!containerEl });

      try {
        loadingMessage = 'Loading emulators...';

        // Dynamically import emulators - it sets window.emulators as a side effect
        await import('emulators');

        // Wait a tick for the global to be set
        await new Promise(resolve => setTimeout(resolve, 0));

        const emulators = window.emulators;
        if (!emulators) {
          throw new Error('Emulators library did not load correctly');
        }

        console.log('[DOSBox] Emulators loaded:', Object.keys(emulators));

        if (cancelled) return;

        // Set the path where WASM files are located
        const wasmBasePath = wdosboxWasmUrl.substring(0, wdosboxWasmUrl.lastIndexOf('/') + 1);
        console.log('[DOSBox] WASM path:', wasmBasePath);
        emulators.pathPrefix = wasmBasePath;

        loadingMessage = 'Creating game bundle...';

        // Create a jsdos bundle (zip with .jsdos/dosbox.conf)
        const { zipSync } = await import('fflate');

        const zipFiles: { [key: string]: Uint8Array } = {};

        // Add all game files
        for (const [path, data] of Object.entries(collectedFiles!.files)) {
          zipFiles[path] = data;
        }

        // Create dosbox.conf that mounts C: and runs the exe
        const dosboxConf = `[sdl]
fullscreen=false
autolock=true

[dosbox]
machine=svga_s3

[cpu]
core=auto
cycles=auto

[mixer]
rate=44100

[autoexec]
@echo off
mount c .
c:
echo Run: ${exeName}
echo.
`;

        // Add required .jsdos configuration
        const encoder = new TextEncoder();
        zipFiles['.jsdos/dosbox.conf'] = encoder.encode(dosboxConf);

        // Create the zip bundle
        const zipData = zipSync(zipFiles);
        console.log('[DOSBox] Bundle created:', zipData.length, 'bytes');

        if (cancelled) return;

        loadingMessage = 'Starting DOSBox...';

        // Use emulators API to run the bundle
        const ci = await emulators.dosboxDirect(zipData);
        dosInstanceRef = ci;

        console.log('[DOSBox] DOSBox started, CI:', ci);

        // Create canvas for rendering
        const canvas = document.createElement('canvas');
        canvas.style.width = '100%';
        canvas.style.height = '100%';
        canvas.style.objectFit = 'contain';
        canvas.style.imageRendering = 'pixelated';
        canvas.style.background = '#000';
        canvas.style.outline = 'none';
        // eslint-disable-next-line svelte/no-dom-manipulating -- DOSBox requires direct DOM manipulation for canvas
        containerEl?.appendChild(canvas);

        // Set up audio context for sound
        const audioContext = new AudioContext();
        const sampleRate = ci.soundFrequency();
        console.log('[DOSBox] Audio sample rate:', sampleRate);

        // Connect the emulator to the canvas
        const ctx = canvas.getContext('2d');
        if (ctx && ci) {
          const events = ci.events();

          // Track canvas size from frame size events
          events.onFrameSize((width: number, height: number) => {
            console.log('[DOSBox] Frame size:', width, height);
            canvas.width = width;
            canvas.height = height;
          });

          // Handle frame rendering
          events.onFrame((rgb: Uint8Array | null, rgba: Uint8Array | null) => {
            const width = ci.width();
            const height = ci.height();

            if (width <= 0 || height <= 0) return;

            // Resize canvas if needed
            if (canvas.width !== width || canvas.height !== height) {
              canvas.width = width;
              canvas.height = height;
            }

            // Prefer RGBA if available, otherwise convert RGB
            if (rgba && rgba.length > 0) {
              const imageData = ctx.createImageData(width, height);
              imageData.data.set(rgba);
              ctx.putImageData(imageData, 0, 0);
            } else if (rgb && rgb.length > 0) {
              const imageData = ctx.createImageData(width, height);
              // Convert RGB to RGBA
              for (let i = 0, j = 0; i < rgb.length; i += 3, j += 4) {
                imageData.data[j] = rgb[i];
                imageData.data[j + 1] = rgb[i + 1];
                imageData.data[j + 2] = rgb[i + 2];
                imageData.data[j + 3] = 255;
              }
              ctx.putImageData(imageData, 0, 0);
            }
          });

          // Handle audio - buffer and play sound samples
          let nextStartTime = 0;

          events.onSoundPush((samples: Float32Array) => {
            if (audioContext.state === 'suspended') {
              audioContext.resume();
            }

            // Create audio buffer and play
            const buffer = audioContext.createBuffer(1, samples.length, sampleRate);
            buffer.getChannelData(0).set(samples);

            const source = audioContext.createBufferSource();
            source.buffer = buffer;
            source.connect(audioContext.destination);

            // Schedule playback to avoid gaps
            const currentTime = audioContext.currentTime;
            const startTime = Math.max(currentTime, nextStartTime);
            source.start(startTime);
            nextStartTime = startTime + buffer.duration;
          });

          // js-dos uses internal key codes, not DOM keyCodes
          const domToJsDos: Record<number, number> = {
            8: 259,    // Backspace
            9: 258,    // Tab
            13: 257,   // Enter
            16: 340,   // Left Shift
            17: 341,   // Left Ctrl
            18: 342,   // Left Alt
            19: 284,   // Pause
            20: 280,   // CapsLock
            27: 256,   // Escape
            32: 32,    // Space
            33: 266,   // PageUp
            34: 267,   // PageDown
            35: 269,   // End
            36: 268,   // Home
            37: 263,   // Left
            38: 265,   // Up
            39: 262,   // Right
            40: 264,   // Down
            45: 260,   // Insert
            46: 261,   // Delete
            // 0-9
            48: 48, 49: 49, 50: 50, 51: 51, 52: 52,
            53: 53, 54: 54, 55: 55, 56: 56, 57: 57,
            // A-Z
            65: 65, 66: 66, 67: 67, 68: 68, 69: 69, 70: 70, 71: 71, 72: 72,
            73: 73, 74: 74, 75: 75, 76: 76, 77: 77, 78: 78, 79: 79, 80: 80,
            81: 81, 82: 82, 83: 83, 84: 84, 85: 85, 86: 86, 87: 87, 88: 88,
            89: 89, 90: 90,
            // F1-F12
            112: 290, 113: 291, 114: 292, 115: 293, 116: 294, 117: 295,
            118: 296, 119: 297, 120: 298, 121: 299, 122: 300, 123: 301,
            // Numpad
            96: 320, 97: 321, 98: 322, 99: 323, 100: 324,
            101: 325, 102: 326, 103: 327, 104: 328, 105: 329,
            106: 332,  // Numpad *
            107: 334,  // Numpad +
            109: 333,  // Numpad -
            110: 330,  // Numpad .
            111: 331,  // Numpad /
            // Other
            144: 282,  // NumLock
            145: 281,  // ScrollLock
            186: 59,   // Semicolon
            187: 61,   // Equals
            188: 44,   // Comma
            189: 45,   // Minus
            190: 46,   // Period
            191: 47,   // Slash
            192: 96,   // Grave/Backtick
            219: 91,   // Left Bracket
            220: 92,   // Backslash
            221: 93,   // Right Bracket
            222: 39,   // Quote
          };

          // Handle keyboard input
          const onKeyDown = (e: KeyboardEvent) => {
            const jsDosCode = domToJsDos[e.keyCode] ?? e.keyCode;
            console.log('[DOSBox] keydown:', e.code, e.key, 'keyCode:', e.keyCode, '-> jsDos:', jsDosCode);
            ci.sendKeyEvent(jsDosCode, true);
            e.preventDefault();
            e.stopPropagation();
          };
          const onKeyUp = (e: KeyboardEvent) => {
            const jsDosCode = domToJsDos[e.keyCode] ?? e.keyCode;
            ci.sendKeyEvent(jsDosCode, false);
            e.preventDefault();
            e.stopPropagation();
          };

          // Handle mouse input with pointer lock
          const onMouseMove = (e: MouseEvent) => {
            if (document.pointerLockElement === canvas) {
              ci.sendMouseRelativeMotion(e.movementX, e.movementY);
            }
          };

          const onMouseDown = (e: MouseEvent) => {
            if (document.pointerLockElement !== canvas) {
              canvas.requestPointerLock();
            }
            ci.sendMouseButton(e.button, true);
          };

          const onMouseUp = (e: MouseEvent) => {
            ci.sendMouseButton(e.button, false);
          };

          // Handle pointer lock change
          const onPointerLockChange = () => {
            if (document.pointerLockElement === canvas) {
              console.log('[DOSBox] Mouse captured');
            } else {
              console.log('[DOSBox] Mouse released');
            }
          };

          canvas.tabIndex = 0;
          canvas.addEventListener('keydown', onKeyDown);
          canvas.addEventListener('keyup', onKeyUp);
          canvas.addEventListener('mousemove', onMouseMove);
          canvas.addEventListener('mousedown', onMouseDown);
          canvas.addEventListener('mouseup', onMouseUp);
          document.addEventListener('pointerlockchange', onPointerLockChange);
          canvas.focus();

          // Resume audio on user interaction
          const resumeAudio = () => {
            if (audioContext.state === 'suspended') {
              audioContext.resume();
            }
          };
          canvas.addEventListener('click', resumeAudio);
          canvas.addEventListener('keydown', resumeAudio, { once: true });

          // Sync files back to hashtree on exit
          const originalHashes = new SvelteMap<string, string>();
          for (const [path, data] of Object.entries(collectedFiles!.files)) {
            const hash = await crypto.subtle.digest('SHA-256', data);
            originalHashes.set(path, toHex(new Uint8Array(hash)));
          }

          events.onExit(async () => {
            console.log('[DOSBox] Syncing files on exit...');
            try {
              const fsTree = await ci.fsTree();

              const syncNode = async (node: FsNode, path: string = '') => {
                const fullPath = path ? `${path}/${node.name}` : node.name;

                if (node.nodes && node.nodes.length > 0) {
                  for (const child of node.nodes) {
                    await syncNode(child, fullPath);
                  }
                } else if (!node.nodes) {
                  if (fullPath.startsWith('.jsdos')) return;

                  try {
                    const content = await ci.fsReadFile(fullPath);
                    const hash = await crypto.subtle.digest('SHA-256', content);
                    const newHash = toHex(new Uint8Array(hash));
                    const origHash = originalHashes.get(fullPath);

                    if (!origHash || origHash !== newHash) {
                      console.log('[DOSBox] Saving:', fullPath);
                      await saveBinaryFile(fullPath, content);
                    }
                  } catch {
                    console.warn('[DOSBox] Could not read:', fullPath);
                  }
                }
              };

              if (fsTree?.nodes) {
                for (const node of fsTree.nodes) {
                  await syncNode(node);
                }
              }
              console.log('[DOSBox] Sync complete');
            } catch (e) {
              console.error('[DOSBox] Sync failed:', e);
            }
          });
        }

        if (!cancelled) {
          status = 'running';
        }

      } catch (err) {
        console.error('[DOSBox] Error:', err);
        if (!cancelled) {
          status = 'error';
          error = err instanceof Error ? err.message : 'Failed to start DOSBox';
        }
      }
    }

    initDosBox();

    return () => {
      cancelled = true;
      if (dosInstanceRef) {
        try {
          dosInstanceRef.exit?.();
        } catch {}
        dosInstanceRef = null;
      }
      if (containerEl) {
        // eslint-disable-next-line svelte/no-dom-manipulating -- cleanup requires direct DOM manipulation
        containerEl.innerHTML = '';
      }
    };
  });

  // Stable hash string for dependency
  let dirHashHex = $derived(directoryCid?.hash ? toHex(directoryCid.hash) : null);

  // Collect directory files when component mounts
  $effect(() => {
    if (!dirHashHex) {
      console.warn('[DOSBox] No directory CID');
      return;
    }

    console.log('[DOSBox] Collecting files from directory:', dirHashHex);
    let cancelled = false;

    async function loadFiles() {
      try {
        const result = await collectDirectoryFiles(
          directoryCid,
          '',
          (msg) => { if (!cancelled) loadingMessage = msg; }
        );
        if (!cancelled) {
          console.log('[DOSBox] Files collected:', result.fileCount, 'files', result.totalSize, 'bytes');
          collectedFiles = result;
        }
      } catch (err) {
        console.error('[DOSBox] Failed to collect files:', err);
        if (!cancelled) {
          error = 'Failed to load directory contents';
          status = 'error';
        }
      }
    }

    loadFiles();
    return () => { cancelled = true; };
  });

  // Cleanup on unmount
  $effect(() => {
    return () => {
      if (dosInstanceRef) {
        try {
          dosInstanceRef.exit?.();
        } catch {}
      }
    };
  });

  function handleExit() {
    if (dosInstanceRef) {
      try {
        dosInstanceRef.exit?.();
      } catch {}
      dosInstanceRef = null;
    }
    status = 'idle';
    onExit?.();
  }
</script>

{#if status === 'idle'}
  <!-- Start screen -->
  <div class="flex-1 flex flex-col items-center justify-center bg-surface-0 min-h-0">
    <div class="text-center p-8 max-w-md">
      <div class="w-16 h-16 mx-auto mb-4 bg-surface-2 rounded-lg flex items-center justify-center">
        <span class="i-lucide-terminal text-3xl text-accent"></span>
      </div>
      <h2 class="text-xl font-medium mb-2">{exeName}</h2>
      <p class="text-sm text-text-2 mb-4">DOS Executable</p>

      {#if collectedFiles}
        <p class="text-xs text-text-3 mb-6">
          {collectedFiles.fileCount} files ({formatBytes(collectedFiles.totalSize)}) ready to mount
        </p>
        <button
          onclick={handleStartClick}
          class="btn-success px-6 py-3"
        >
          <span class="i-lucide-play mr-2"></span>
          Run in DOSBox
        </button>
      {:else}
        <div class="flex items-center justify-center gap-2 text-text-2">
          <span class="i-lucide-loader-2 animate-spin"></span>
          <span class="text-sm">{loadingMessage || 'Loading files...'}</span>
        </div>
      {/if}

      <p class="text-xs text-text-3 mt-6">
        Powered by js-dos (DOSBox in WebAssembly)
      </p>
    </div>
  </div>
{:else if status === 'error'}
  <!-- Error screen -->
  <div class="flex-1 flex flex-col items-center justify-center bg-surface-0 p-8 min-h-0">
    <div class="w-16 h-16 mx-auto mb-4 bg-danger/10 rounded-lg flex items-center justify-center">
      <span class="i-lucide-x text-3xl text-danger"></span>
    </div>
    <h2 class="text-xl font-medium mb-2">DOSBox Error</h2>
    <p class="text-sm text-text-2 mb-4">{error}</p>
    <button
      onclick={() => { status = 'idle'; shouldStart = false; }}
      class="btn-ghost"
    >
      Try Again
    </button>
  </div>
{:else}
  <!-- Loading or Running state -->
  <div class="flex-1 flex flex-col bg-black min-h-0">
    <!-- Toolbar -->
    <div class="h-10 shrink-0 px-3 flex items-center justify-between bg-surface-1 border-b border-surface-3 {isFullscreen ? 'absolute top-0 left-0 right-0 z-10 opacity-0 hover:opacity-100 transition-opacity' : ''}">
      <span class="text-sm font-mono flex items-center gap-2">
        <span class="i-lucide-terminal text-accent"></span>
        {exeName}
        {#if status === 'loading'}
          <span class="text-xs text-text-2 ml-2">{loadingMessage}</span>
        {/if}
      </span>
      <div class="flex items-center gap-2">
        <button
          onclick={toggleFullscreen}
          class="btn-ghost text-sm"
          title={isFullscreen ? 'Exit fullscreen (Esc)' : 'Fullscreen'}
        >
          <span class={isFullscreen ? 'i-lucide-minimize' : 'i-lucide-maximize'}></span>
        </button>
        <button
          onclick={handleExit}
          class="btn-ghost text-sm"
        >
          <span class="i-lucide-x mr-1"></span>
          Exit
        </button>
      </div>
    </div>

    <!-- DOSBox canvas container - js-dos renders into this -->
    <div
      bind:this={containerEl}
      class="flex-1 bg-black relative min-h-0 outline-none"
    >
      {#if status === 'loading'}
        <div class="absolute inset-0 flex flex-col items-center justify-center text-green-400">
          <span class="i-lucide-loader-2 text-4xl animate-spin mb-4"></span>
          <p class="text-sm">{loadingMessage}</p>
        </div>
      {/if}
    </div>
  </div>
{/if}
