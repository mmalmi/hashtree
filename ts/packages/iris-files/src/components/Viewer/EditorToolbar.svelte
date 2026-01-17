<script lang="ts">
  /**
   * EditorToolbar - Formatting toolbar for Tiptap editor
   */
  import type { Editor } from '@tiptap/core';

  interface Props {
    editor: Editor;
    hasTextSelection: boolean;
    userNpub: string | null;
    commentsCount: number;
    commentsPanelOpen: boolean;
    onAddComment: () => void;
    onToggleCommentsPanel: () => void;
    onImageUpload: () => void;
  }

  let {
    editor,
    hasTextSelection,
    userNpub,
    commentsCount,
    commentsPanelOpen,
    onAddComment,
    onToggleCommentsPanel,
    onImageUpload,
  }: Props = $props();

  // Formatting toolbar actions
  function toggleBold() { editor.chain().focus().toggleBold().run(); }
  function toggleItalic() { editor.chain().focus().toggleItalic().run(); }
  function toggleStrike() { editor.chain().focus().toggleStrike().run(); }
  function toggleCode() { editor.chain().focus().toggleCode().run(); }
  function toggleHeading1() { editor.chain().focus().toggleHeading({ level: 1 }).run(); }
  function toggleHeading2() { editor.chain().focus().toggleHeading({ level: 2 }).run(); }
  function toggleHeading3() { editor.chain().focus().toggleHeading({ level: 3 }).run(); }
  function toggleBulletList() { editor.chain().focus().toggleBulletList().run(); }
  function toggleOrderedList() { editor.chain().focus().toggleOrderedList().run(); }
  function toggleBlockquote() { editor.chain().focus().toggleBlockquote().run(); }
  function toggleCodeBlock() { editor.chain().focus().toggleCodeBlock().run(); }
  function insertHorizontalRule() { editor.chain().focus().setHorizontalRule().run(); }
  function undo() { editor.chain().focus().undo().run(); }
  function redo() { editor.chain().focus().redo().run(); }
</script>

<div class="flex items-center justify-center gap-1 px-4 py-2 border-b border-surface-3 bg-surface-1 shrink-0 flex-wrap">
  <!-- Text formatting -->
  <button
    onclick={toggleBold}
    class="toolbar-btn {editor.isActive('bold') ? 'active' : ''}"
    title="Bold (Ctrl+B)"
  >
    <span class="i-lucide-bold"></span>
  </button>
  <button
    onclick={toggleItalic}
    class="toolbar-btn {editor.isActive('italic') ? 'active' : ''}"
    title="Italic (Ctrl+I)"
  >
    <span class="i-lucide-italic"></span>
  </button>
  <button
    onclick={toggleStrike}
    class="toolbar-btn {editor.isActive('strike') ? 'active' : ''}"
    title="Strikethrough"
  >
    <span class="i-lucide-strikethrough"></span>
  </button>
  <button
    onclick={toggleCode}
    class="toolbar-btn {editor.isActive('code') ? 'active' : ''}"
    title="Inline Code"
  >
    <span class="i-lucide-code"></span>
  </button>

  <div class="w-px h-5 bg-surface-3 mx-1"></div>

  <!-- Headings -->
  <button
    onclick={toggleHeading1}
    class="toolbar-btn {editor.isActive('heading', { level: 1 }) ? 'active' : ''}"
    title="Heading 1"
  >
    <span class="i-lucide-heading-1"></span>
  </button>
  <button
    onclick={toggleHeading2}
    class="toolbar-btn {editor.isActive('heading', { level: 2 }) ? 'active' : ''}"
    title="Heading 2"
  >
    <span class="i-lucide-heading-2"></span>
  </button>
  <button
    onclick={toggleHeading3}
    class="toolbar-btn {editor.isActive('heading', { level: 3 }) ? 'active' : ''}"
    title="Heading 3"
  >
    <span class="i-lucide-heading-3"></span>
  </button>

  <div class="w-px h-5 bg-surface-3 mx-1"></div>

  <!-- Lists -->
  <button
    onclick={toggleBulletList}
    class="toolbar-btn {editor.isActive('bulletList') ? 'active' : ''}"
    title="Bullet List"
  >
    <span class="i-lucide-list"></span>
  </button>
  <button
    onclick={toggleOrderedList}
    class="toolbar-btn {editor.isActive('orderedList') ? 'active' : ''}"
    title="Numbered List"
  >
    <span class="i-lucide-list-ordered"></span>
  </button>

  <div class="w-px h-5 bg-surface-3 mx-1"></div>

  <!-- Block elements -->
  <button
    onclick={toggleBlockquote}
    class="toolbar-btn {editor.isActive('blockquote') ? 'active' : ''}"
    title="Quote"
  >
    <span class="i-lucide-quote"></span>
  </button>
  <button
    onclick={toggleCodeBlock}
    class="toolbar-btn {editor.isActive('codeBlock') ? 'active' : ''}"
    title="Code Block"
  >
    <span class="i-lucide-file-code"></span>
  </button>
  <button
    onclick={insertHorizontalRule}
    class="toolbar-btn"
    title="Horizontal Rule"
  >
    <span class="i-lucide-minus"></span>
  </button>
  <button
    onclick={onImageUpload}
    class="toolbar-btn"
    title="Insert Image"
  >
    <span class="i-lucide-image"></span>
  </button>

  <div class="w-px h-5 bg-surface-3 mx-1"></div>

  <!-- Undo/Redo -->
  <button
    onclick={undo}
    disabled={!editor.can().undo()}
    class="toolbar-btn disabled:opacity-30"
    title="Undo (Ctrl+Z)"
  >
    <span class="i-lucide-undo"></span>
  </button>
  <button
    onclick={redo}
    disabled={!editor.can().redo()}
    class="toolbar-btn disabled:opacity-30"
    title="Redo (Ctrl+Shift+Z)"
  >
    <span class="i-lucide-redo"></span>
  </button>

  <div class="w-px h-5 bg-surface-3 mx-1"></div>

  <!-- Comments -->
  <button
    onclick={onAddComment}
    disabled={!hasTextSelection || !userNpub}
    class="toolbar-btn disabled:opacity-30"
    title="Add comment (select text first)"
  >
    <span class="i-lucide-message-square-plus"></span>
  </button>
  <button
    onclick={onToggleCommentsPanel}
    class="toolbar-btn {commentsPanelOpen ? 'active' : ''}"
    title="Toggle comments panel"
  >
    <span class="i-lucide-message-square"></span>
    {#if commentsCount > 0}
      <span class="text-xs ml-1">{commentsCount}</span>
    {/if}
  </button>
</div>

<style>
  /* Toolbar button styles */
  .toolbar-btn {
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0.5rem;
    border-radius: 0.25rem;
    background: transparent;
    border: none;
    color: var(--color-text-1);
    cursor: pointer;
    transition: background-color 0.15s, color 0.15s;
  }

  .toolbar-btn:hover {
    background: var(--color-surface-2);
  }

  .toolbar-btn.active {
    background: var(--color-surface-3);
    color: var(--color-accent);
  }

  .toolbar-btn:disabled {
    opacity: 0.3;
    cursor: not-allowed;
  }

  .toolbar-btn span {
    font-size: 1rem;
  }
</style>
