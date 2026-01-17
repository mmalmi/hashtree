/**
 * Comments Store
 * Manages comment threads using Yjs for real-time collaboration
 */
import * as Y from 'yjs';
import type { CommentThread, CommentReply, CommentsState } from './types';
import { generateCommentId } from './types';

export interface CommentsStore {
  /** Get the current state */
  getState(): CommentsState;
  /** Subscribe to state changes */
  subscribe(callback: (state: CommentsState) => void): () => void;
  /** Create a new comment thread */
  createThread(quotedText: string, content: string, authorNpub: string): string;
  /** Add a reply to an existing thread */
  addReply(threadId: string, content: string, authorNpub: string): void;
  /** Resolve a thread */
  resolveThread(threadId: string): void;
  /** Unresolve a thread */
  unresolveThread(threadId: string): void;
  /** Delete a thread */
  deleteThread(threadId: string): void;
  /** Delete a specific comment from a thread */
  deleteComment(threadId: string, commentId: string): void;
  /** Set the active thread */
  setActiveThread(threadId: string | null): void;
  /** Toggle the comments panel */
  togglePanel(): void;
  /** Set panel open state */
  setPanelOpen(open: boolean): void;
  /** Get a thread by ID */
  getThread(threadId: string): CommentThread | undefined;
  /** Get the Yjs map for persistence */
  getYMap(): Y.Map<unknown>;
  /** Destroy the store and clean up subscriptions */
  destroy(): void;
}

/**
 * Create a comments store backed by Yjs for real-time sync
 */
export function createCommentsStore(ydoc: Y.Doc): CommentsStore {
  // Get or create the comments map in the Yjs document
  const ycomments = ydoc.getMap<Y.Map<unknown>>('comments');

  // Local state for UI (not synced via Yjs)
  let activeThreadId: string | null = null;
  let panelOpen = false;

  // Subscribers
  const subscribers = new Set<(state: CommentsState) => void>();

  // Convert Yjs data to CommentThread
  function yMapToThread(ythread: Y.Map<unknown>): CommentThread {
    const comments: CommentReply[] = [];
    const ycommentsList = ythread.get('comments') as Y.Array<Y.Map<unknown>> | undefined;

    if (ycommentsList) {
      ycommentsList.forEach((ycomment: Y.Map<unknown>) => {
        comments.push({
          id: ycomment.get('id') as string,
          content: ycomment.get('content') as string,
          authorNpub: ycomment.get('authorNpub') as string,
          createdAt: ycomment.get('createdAt') as number,
        });
      });
    }

    return {
      id: ythread.get('id') as string,
      quotedText: ythread.get('quotedText') as string,
      resolved: ythread.get('resolved') as boolean || false,
      comments,
      createdAt: ythread.get('createdAt') as number,
      updatedAt: ythread.get('updatedAt') as number,
    };
  }

  // Get current state
  function getState(): CommentsState {
    const threads = new Map<string, CommentThread>();

    ycomments.forEach((ythread, threadId) => {
      if (ythread instanceof Y.Map) {
        threads.set(threadId, yMapToThread(ythread));
      }
    });

    return {
      threads,
      activeThreadId,
      panelOpen,
    };
  }

  // Notify subscribers
  function notify() {
    const state = getState();
    subscribers.forEach(cb => cb(state));
  }

  // Subscribe to Yjs changes
  const yObserver = () => notify();
  ycomments.observeDeep(yObserver);

  return {
    getState,

    subscribe(callback) {
      subscribers.add(callback);
      // Immediately call with current state
      callback(getState());
      return () => {
        subscribers.delete(callback);
      };
    },

    createThread(quotedText: string, content: string, authorNpub: string): string {
      const threadId = generateCommentId();
      const now = Date.now();

      const ythread = new Y.Map<unknown>();
      ythread.set('id', threadId);
      ythread.set('quotedText', quotedText);
      ythread.set('resolved', false);
      ythread.set('createdAt', now);
      ythread.set('updatedAt', now);

      // Create the comments array with the first comment
      const ycommentsList = new Y.Array<Y.Map<unknown>>();
      const firstComment = new Y.Map<unknown>();
      firstComment.set('id', generateCommentId());
      firstComment.set('content', content);
      firstComment.set('authorNpub', authorNpub);
      firstComment.set('createdAt', now);
      ycommentsList.push([firstComment]);

      ythread.set('comments', ycommentsList);

      // Add to the comments map
      ycomments.set(threadId, ythread);

      // Set as active and open panel
      activeThreadId = threadId;
      panelOpen = true;
      notify();

      return threadId;
    },

    addReply(threadId: string, content: string, authorNpub: string) {
      const ythread = ycomments.get(threadId);
      if (!ythread || !(ythread instanceof Y.Map)) return;

      const ycommentsList = ythread.get('comments') as Y.Array<Y.Map<unknown>> | undefined;
      if (!ycommentsList) return;

      const now = Date.now();
      const reply = new Y.Map<unknown>();
      reply.set('id', generateCommentId());
      reply.set('content', content);
      reply.set('authorNpub', authorNpub);
      reply.set('createdAt', now);

      ycommentsList.push([reply]);
      ythread.set('updatedAt', now);
    },

    resolveThread(threadId: string) {
      const ythread = ycomments.get(threadId);
      if (!ythread || !(ythread instanceof Y.Map)) return;

      ythread.set('resolved', true);
      ythread.set('updatedAt', Date.now());
    },

    unresolveThread(threadId: string) {
      const ythread = ycomments.get(threadId);
      if (!ythread || !(ythread instanceof Y.Map)) return;

      ythread.set('resolved', false);
      ythread.set('updatedAt', Date.now());
    },

    deleteThread(threadId: string) {
      ycomments.delete(threadId);
      if (activeThreadId === threadId) {
        activeThreadId = null;
        notify();
      }
    },

    deleteComment(threadId: string, commentId: string) {
      const ythread = ycomments.get(threadId);
      if (!ythread || !(ythread instanceof Y.Map)) return;

      const ycommentsList = ythread.get('comments') as Y.Array<Y.Map<unknown>> | undefined;
      if (!ycommentsList) return;

      // Find and remove the comment
      let indexToRemove = -1;
      ycommentsList.forEach((ycomment: Y.Map<unknown>, index: number) => {
        if (ycomment.get('id') === commentId) {
          indexToRemove = index;
        }
      });

      if (indexToRemove !== -1) {
        // If it's the only comment, delete the whole thread
        if (ycommentsList.length === 1) {
          ycomments.delete(threadId);
          if (activeThreadId === threadId) {
            activeThreadId = null;
            notify();
          }
        } else {
          ycommentsList.delete(indexToRemove, 1);
          ythread.set('updatedAt', Date.now());
        }
      }
    },

    setActiveThread(threadId: string | null) {
      activeThreadId = threadId;
      notify();
    },

    togglePanel() {
      panelOpen = !panelOpen;
      notify();
    },

    setPanelOpen(open: boolean) {
      panelOpen = open;
      notify();
    },

    getThread(threadId: string): CommentThread | undefined {
      const ythread = ycomments.get(threadId);
      if (!ythread || !(ythread instanceof Y.Map)) return undefined;
      return yMapToThread(ythread);
    },

    getYMap(): Y.Map<unknown> {
      return ycomments as Y.Map<unknown>;
    },

    destroy() {
      ycomments.unobserveDeep(yObserver);
      subscribers.clear();
    },
  };
}
