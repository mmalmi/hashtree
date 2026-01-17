/**
 * Tiptap Comment Mark Extension
 * Adds a mark type for highlighting commented text in the editor
 */
import { Mark, mergeAttributes } from '@tiptap/core';

export interface CommentMarkOptions {
  HTMLAttributes: Record<string, unknown>;
  onCommentActivated?: (commentId: string | null) => void;
}

declare module '@tiptap/core' {
  interface Commands<ReturnType> {
    comment: {
      /**
       * Set a comment mark on the current selection
       */
      setComment: (commentId: string) => ReturnType;
      /**
       * Remove comment mark from the current selection
       */
      unsetComment: () => ReturnType;
      /**
       * Toggle comment mark
       */
      toggleComment: (commentId: string) => ReturnType;
      /**
       * Remove all comment marks with the given commentId from the document
       */
      removeCommentById: (commentId: string) => ReturnType;
    };
  }
}

export const CommentMark = Mark.create<CommentMarkOptions>({
  name: 'comment',

  addOptions() {
    return {
      HTMLAttributes: {},
      onCommentActivated: undefined,
    };
  },

  addAttributes() {
    return {
      commentId: {
        default: null,
        parseHTML: (element) => element.getAttribute('data-comment-id'),
        renderHTML: (attributes) => {
          if (!attributes.commentId) {
            return {};
          }
          return {
            'data-comment-id': attributes.commentId,
          };
        },
      },
    };
  },

  parseHTML() {
    return [
      {
        tag: 'span[data-comment-id]',
      },
    ];
  },

  renderHTML({ HTMLAttributes }) {
    return [
      'span',
      mergeAttributes(this.options.HTMLAttributes, HTMLAttributes, {
        class: 'comment-highlight',
      }),
      0,
    ];
  },

  addCommands() {
    return {
      setComment:
        (commentId: string) =>
        ({ commands }) => {
          return commands.setMark(this.name, { commentId });
        },
      unsetComment:
        () =>
        ({ commands }) => {
          return commands.unsetMark(this.name);
        },
      toggleComment:
        (commentId: string) =>
        ({ commands }) => {
          return commands.toggleMark(this.name, { commentId });
        },
      removeCommentById:
        (commentId: string) =>
        ({ tr, state, dispatch }) => {
          const markType = state.schema.marks[this.name];
          if (!markType) return false;

          // Find all positions with this comment mark
          const positionsToRemove: { from: number; to: number }[] = [];

          state.doc.descendants((node, pos) => {
            const mark = node.marks.find(
              (m) => m.type === markType && m.attrs.commentId === commentId
            );
            if (mark) {
              positionsToRemove.push({ from: pos, to: pos + node.nodeSize });
            }
          });

          if (positionsToRemove.length === 0) return false;

          // Remove marks from all positions
          for (const { from, to } of positionsToRemove) {
            tr.removeMark(from, to, markType);
          }

          if (dispatch) {
            dispatch(tr);
          }

          return true;
        },
    };
  },
});
