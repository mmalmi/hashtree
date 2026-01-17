<script lang="ts">
  /**
   * VideoDescription - Renders video description with clickable timestamps
   * Timestamps like 00:00, 1:30, 1:30:00 become clickable links that seek the video
   */
  import { getQueryParams } from '../../lib/router.svelte';

  interface Props {
    text: string;
    maxLines?: number;
    maxChars?: number;
    class?: string;
    style?: string;
    /** Optional timestamp to display at the top (formatted string like "2 days ago") */
    timestamp?: string;
  }

  let { text, maxLines = 4, maxChars = 400, class: className = '', style = '', timestamp }: Props = $props();

  // Segment types
  type TextSegment =
    | { type: 'text'; content: string }
    | { type: 'link'; url: string }
    | { type: 'timestamp'; display: string; seconds: number };

  // URL regex - matches http(s) URLs
  const URL_REGEX = /https?:\/\/[^\s<>"{}|\\^`\[\]]+/g;

  // Timestamp regex - matches HH:MM:SS, H:MM:SS, MM:SS, M:SS at word boundaries
  // Must be at start of line or after whitespace to avoid matching random numbers
  const TIMESTAMP_REGEX = /(?:^|(?<=\s))(\d{1,2}:\d{2}(?::\d{2})?)(?=\s|$)/gm;

  /** Parse timestamp string to seconds */
  function parseTimestamp(ts: string): number {
    const parts = ts.split(':').map(Number);
    if (parts.length === 3) {
      // HH:MM:SS
      return parts[0] * 3600 + parts[1] * 60 + parts[2];
    } else if (parts.length === 2) {
      // MM:SS
      return parts[0] * 60 + parts[1];
    }
    return 0;
  }

  /** Parse text into segments of plain text, links, and timestamps */
  function parseSegments(input: string): TextSegment[] {
    const segments: TextSegment[] = [];

    // First, find all URLs and timestamps with their positions
    type Match = { type: 'link' | 'timestamp'; start: number; end: number; content: string; seconds?: number };
    const matches: Match[] = [];

    // Find URLs
    URL_REGEX.lastIndex = 0;
    let match;
    while ((match = URL_REGEX.exec(input)) !== null) {
      let url = match[0];
      // Clean up trailing punctuation
      const trailingPunct = /[.,;:!?)]+$/;
      const punctMatch = url.match(trailingPunct);
      if (punctMatch) {
        url = url.slice(0, -punctMatch[0].length);
      }
      matches.push({ type: 'link', start: match.index, end: match.index + url.length, content: url });
    }

    // Find timestamps
    TIMESTAMP_REGEX.lastIndex = 0;
    while ((match = TIMESTAMP_REGEX.exec(input)) !== null) {
      const ts = match[1];
      const start = match.index + (match[0].length - ts.length); // Adjust for lookbehind
      matches.push({
        type: 'timestamp',
        start,
        end: start + ts.length,
        content: ts,
        seconds: parseTimestamp(ts)
      });
    }

    // Sort by position
    matches.sort((a, b) => a.start - b.start);

    // Remove overlapping matches (URLs take priority)
    const filtered: Match[] = [];
    let lastEnd = 0;
    for (const m of matches) {
      if (m.start >= lastEnd) {
        filtered.push(m);
        lastEnd = m.end;
      }
    }

    // Build segments
    let lastIndex = 0;
    for (const m of filtered) {
      // Add text before this match
      if (m.start > lastIndex) {
        segments.push({ type: 'text', content: input.slice(lastIndex, m.start) });
      }

      if (m.type === 'link') {
        segments.push({ type: 'link', url: m.content });
      } else {
        segments.push({ type: 'timestamp', display: m.content, seconds: m.seconds! });
      }

      lastIndex = m.end;
    }

    // Add remaining text
    if (lastIndex < input.length) {
      segments.push({ type: 'text', content: input.slice(lastIndex) });
    }

    return segments;
  }

  /** Handle timestamp click - update URL with ?t= param */
  function handleTimestampClick(seconds: number) {
    const hash = window.location.hash;
    const qIdx = hash.indexOf('?');
    const path = qIdx !== -1 ? hash.slice(0, qIdx) : hash;
    const params = getQueryParams();
    params.set('t', seconds.toString());
    window.location.hash = `${path}?${params.toString()}`;
  }

  let expanded = $state(false);

  // Check if text needs truncation
  let lines = $derived(text.split('\n'));
  let needsTruncation = $derived(lines.length > maxLines || text.length > maxChars);

  // Truncated text
  let displayText = $derived.by(() => {
    if (expanded || !needsTruncation) return text;

    // Truncate by lines first
    let truncated = lines.slice(0, maxLines).join('\n');

    // Then by chars if still too long
    if (truncated.length > maxChars) {
      truncated = truncated.slice(0, maxChars);
      // Don't cut in middle of word
      const lastSpace = truncated.lastIndexOf(' ');
      if (lastSpace > maxChars * 0.8) {
        truncated = truncated.slice(0, lastSpace);
      }
    }

    return truncated;
  });

  let isTruncated = $derived(!expanded && needsTruncation);
  let segments = $derived(parseSegments(displayText));
</script>

<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div
  class="{className} rounded-lg p-3 {isTruncated ? 'cursor-pointer transition-colors duration-100' : ''}"
  {style}
  onmouseenter={(e) => { if (isTruncated) e.currentTarget.style.backgroundColor = 'var(--desc-hover-color, rgba(255,255,255,0.15))'; }}
  onmouseleave={(e) => { if (isTruncated) e.currentTarget.style.backgroundColor = ''; }}
  onclick={() => { if (isTruncated) expanded = true; }}
>
  {#if timestamp}
    <p class="font-semibold text-text-1 mb-1">{timestamp}</p>
  {/if}
  <p class="whitespace-pre-wrap break-words">{#each segments as segment, i (i)}{#if segment.type === 'link'}<a href={segment.url} target="_blank" rel="noopener noreferrer" class="text-accent hover:underline" onclick={(e) => e.stopPropagation()}>{segment.url}</a>{:else if segment.type === 'timestamp'}<button
      onclick={(e) => { e.stopPropagation(); handleTimestampClick(segment.seconds); }}
      class="text-accent hover:underline cursor-pointer bg-transparent border-none p-0 font-inherit"
    >{segment.display}</button>{:else}{segment.content}{/if}{/each}{#if isTruncated}...{/if}</p>
  {#if expanded && needsTruncation}
    <button
      onclick={(e) => { e.stopPropagation(); expanded = false; }}
      class="text-accent hover:underline text-sm mt-1"
    >
      Show less
    </button>
  {/if}
</div>
