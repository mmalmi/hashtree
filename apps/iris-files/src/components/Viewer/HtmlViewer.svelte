<script lang="ts">
  /**
   * HtmlViewer - renders HTML content in a sandboxed iframe
   *
   * Injects a <base> tag so relative URLs resolve to SW paths,
   * but serves HTML as blob to maintain sandbox security (no same-origin).
   * The SW then intercepts resource requests and serves from hashtree.
   */
  import { untrack } from 'svelte';
  import { routeStore, currentDirCidStore } from '../../stores';
  import { getTree } from '../../store';

  interface Props {
    content: string;
    fileName: string;
  }

  let { content, fileName }: Props = $props();

  let route = $derived($routeStore);
  let currentDirCid = $derived($currentDirCidStore);

  // Build base URL for the directory containing the HTML file
  // e.g., /htree/npub1.../treeName/path/to/ (trailing slash for directory)
  let baseUrl = $derived.by(() => {
    if (!route.npub || !route.treeName) return '';

    const encodedTreeName = encodeURIComponent(route.treeName);
    // Get directory path (all segments except the filename)
    const dirPath = route.path.slice(0, -1);
    const encodedPath = dirPath.map(encodeURIComponent).join('/');

    // Build base URL with trailing slash
    let base = `/htree/${route.npub}/${encodedTreeName}`;
    if (encodedPath) {
      base += `/${encodedPath}`;
    }
    return base + '/';
  });

  let iframeSrc = $state<string>('');

  function isRelativeResource(href: string | null): href is string {
    if (!href) return false;
    if (href.startsWith('/')) return false;
    if (href.startsWith('//')) return false;
    if (href.startsWith('data:') || href.startsWith('blob:')) return false;
    return !/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(href);
  }

  function normalizeRelativePath(href: string): string[] {
    const parts = href.split('/').filter(part => part.length > 0);
    const stack: string[] = [];
    for (const part of parts) {
      if (part === '.') continue;
      if (part === '..') {
        stack.pop();
        continue;
      }
      stack.push(part);
    }
    return stack;
  }

  async function buildHtml(contentValue: string, baseHref: string, dirCid: typeof currentDirCid): Promise<string> {
    if (typeof DOMParser === 'undefined') {
      return contentValue;
    }

    const doc = new DOMParser().parseFromString(contentValue, 'text/html');
    if (!doc.documentElement) return contentValue;

    let head = doc.querySelector('head');
    if (!head) {
      head = doc.createElement('head');
      doc.documentElement.prepend(head);
    }

    const existingBase = head.querySelector('base');
    if (existingBase) {
      existingBase.setAttribute('href', baseHref);
    } else {
      const baseEl = doc.createElement('base');
      baseEl.setAttribute('href', baseHref);
      head.prepend(baseEl);
    }

    if (!dirCid) {
      const doctype = doc.doctype ? `<!DOCTYPE ${doc.doctype.name}>` : '';
      return doctype + doc.documentElement.outerHTML;
    }

    const tree = getTree();
    const decoder = new TextDecoder('utf-8');

    const styles = Array.from(doc.querySelectorAll('link[rel="stylesheet"][href]'));
    for (const link of styles) {
      const href = link.getAttribute('href');
      if (!isRelativeResource(href)) continue;
      const parts = normalizeRelativePath(href);
      if (parts.length === 0) continue;
      try {
        const resolved = await tree.resolvePath(dirCid, parts);
        if (!resolved) continue;
        const data = await tree.readFile(resolved.cid);
        if (!data) continue;
        const styleEl = doc.createElement('style');
        styleEl.textContent = decoder.decode(data);
        link.replaceWith(styleEl);
      } catch {
        // Keep original link if inlining fails
      }
    }

    const scripts = Array.from(doc.querySelectorAll('script[src]'));
    for (const script of scripts) {
      const src = script.getAttribute('src');
      if (!isRelativeResource(src)) continue;
      const parts = normalizeRelativePath(src);
      if (parts.length === 0) continue;
      try {
        const resolved = await tree.resolvePath(dirCid, parts);
        if (!resolved) continue;
        const data = await tree.readFile(resolved.cid);
        if (!data) continue;
        const inlineScript = doc.createElement('script');
        const type = script.getAttribute('type');
        if (type) inlineScript.setAttribute('type', type);
        inlineScript.textContent = decoder.decode(data);
        script.replaceWith(inlineScript);
      } catch {
        // Keep original script if inlining fails
      }
    }

    const doctype = doc.doctype ? `<!DOCTYPE ${doc.doctype.name}>` : '';
    return doctype + doc.documentElement.outerHTML;
  }

  // Inject <base> tag into HTML, inline local resources, and create blob URL
  $effect(() => {
    if (!content || !baseUrl) {
      return;
    }

    let cancelled = false;
    let localSrc = '';
    const dirCid = currentDirCid;

    void (async () => {
      const modifiedHtml = await buildHtml(content, baseUrl, dirCid);
      if (cancelled) return;

      // Create blob URL for the modified HTML
      const blob = new Blob([modifiedHtml], { type: 'text/html' });
      const newSrc = URL.createObjectURL(blob);
      localSrc = newSrc;

      // Store old URL for cleanup before setting new one (use untrack to avoid dependency)
      const oldSrc = untrack(() => iframeSrc);
      iframeSrc = newSrc;

      // Cleanup: revoke old blob URL
      if (oldSrc) {
        URL.revokeObjectURL(oldSrc);
      }
    })();

    return () => {
      cancelled = true;
      if (localSrc) {
        URL.revokeObjectURL(localSrc);
      }
    };
  });
</script>

<div class="flex-1 flex flex-col min-h-0">
  {#if iframeSrc}
    <iframe
      src={iframeSrc}
      class="flex-1 w-full border-0 bg-white"
      sandbox="allow-scripts"
      title={fileName}
    ></iframe>
  {:else}
    <div class="flex-1 flex items-center justify-center text-muted">
      Loading...
    </div>
  {/if}
</div>
