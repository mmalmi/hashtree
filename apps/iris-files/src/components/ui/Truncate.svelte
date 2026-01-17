<script lang="ts">
  /**
   * Truncate - Show more/less for long text content
   * Truncates by line count or character count
   * Automatically highlights URLs as clickable links
   */

  type TextSegment = { type: 'text'; content: string } | { type: 'link'; url: string };

  interface Props {
    text: string;
    maxLines?: number;
    maxChars?: number;
    class?: string;
    highlightLinks?: boolean;
  }

  let { text, maxLines = 3, maxChars = 300, class: className = '', highlightLinks = true }: Props = $props();

  // URL regex - matches http(s) URLs
  const URL_REGEX = /https?:\/\/[^\s<>"{}|\\^`\[\]]+/g;

  /** Parse text into segments of plain text and links */
  function parseSegments(input: string): TextSegment[] {
    if (!highlightLinks) return [{ type: 'text', content: input }];

    const segments: TextSegment[] = [];
    let lastIndex = 0;

    // Reset regex state
    URL_REGEX.lastIndex = 0;

    let match;
    while ((match = URL_REGEX.exec(input)) !== null) {
      // Add text before this URL
      if (match.index > lastIndex) {
        segments.push({ type: 'text', content: input.slice(lastIndex, match.index) });
      }

      // Clean up trailing punctuation that's likely not part of URL
      let url = match[0];
      const trailingPunct = /[.,;:!?)]+$/;
      const punctMatch = url.match(trailingPunct);
      let suffix = '';
      if (punctMatch) {
        suffix = punctMatch[0];
        url = url.slice(0, -suffix.length);
      }

      // Add the link
      segments.push({ type: 'link', url });

      // Add the trailing punctuation as text if any
      if (suffix) {
        segments.push({ type: 'text', content: suffix });
      }

      lastIndex = match.index + match[0].length;
    }

    // Add remaining text
    if (lastIndex < input.length) {
      segments.push({ type: 'text', content: input.slice(lastIndex) });
    }

    return segments;
  }

  let expanded = $state(false);

  // Check if text needs truncation
  let lines = $derived(text.split('\n'));
  let needsTruncation = $derived(lines.length > maxLines || text.length > maxChars);

  // Show top button when expanded and content is really long
  let showTopButton = $derived(expanded && (lines.length > maxLines * 2 || text.length > maxChars * 2));

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

<div class={className}>
  {#if showTopButton}
    <button
      onclick={() => expanded = false}
      class="text-accent hover:underline text-sm mb-2"
    >
      Show less
    </button>
  {/if}
  <p class="whitespace-pre-wrap break-words">{#each segments as segment (segment.type === 'link' ? segment.url : segment.content)}{#if segment.type === 'link'}<a href={segment.url} target="_blank" rel="noopener noreferrer" class="text-accent hover:underline">{segment.url}</a>{:else}{segment.content}{/if}{/each}{#if isTruncated}...{/if}</p>
  {#if needsTruncation}
    <button
      onclick={() => expanded = !expanded}
      class="text-accent hover:underline text-sm mt-1"
    >
      {expanded ? 'Show less' : 'Show more'}
    </button>
  {/if}
</div>
