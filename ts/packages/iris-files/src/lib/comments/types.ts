/**
 * Document Comments Types
 * Google Docs-style commenting for Yjs collaborative documents
 */

/**
 * A single comment reply in a thread
 */
export interface CommentReply {
  id: string;
  content: string;
  authorNpub: string;
  createdAt: number;
}

/**
 * A comment thread anchored to text in the document
 */
export interface CommentThread {
  id: string;
  /** The original highlighted text when the comment was created */
  quotedText: string;
  /** Whether the thread has been resolved */
  resolved: boolean;
  /** All comments in this thread (first one is the original comment) */
  comments: CommentReply[];
  /** When the thread was created */
  createdAt: number;
  /** When the thread was last updated */
  updatedAt: number;
}

/**
 * Comment data stored in the Tiptap mark
 */
export interface CommentMarkData {
  commentId: string;
}

/**
 * State of the comments system
 */
export interface CommentsState {
  /** All comment threads, keyed by thread ID */
  threads: Map<string, CommentThread>;
  /** Currently active/selected thread ID */
  activeThreadId: string | null;
  /** Whether the comments panel is open */
  panelOpen: boolean;
}

/**
 * Generate a unique comment ID
 */
export function generateCommentId(): string {
  const timestamp = Date.now().toString(36);
  const random = Math.random().toString(36).slice(2, 8);
  return `c-${timestamp}-${random}`;
}
