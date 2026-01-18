import { defineConfig, presetUno, presetIcons } from 'unocss';
import presetTypography from '@unocss/preset-typography';

export default defineConfig({
  safelist: [
    'animate-pulse-live',
    'animate-pulse-bg',
    'animate-fade-in',
    // Video thumbnail aspect ratio
    'aspect-video',
    // YjsDocument toolbar icons
    'i-lucide-bold',
    'i-lucide-italic',
    'i-lucide-strikethrough',
    'i-lucide-code',
    'i-lucide-heading-1',
    'i-lucide-heading-2',
    'i-lucide-heading-3',
    'i-lucide-list',
    'i-lucide-list-ordered',
    'i-lucide-quote',
    'i-lucide-file-code',
    'i-lucide-minus',
    'i-lucide-undo',
    'i-lucide-redo',
    // CollaboratorsModal and QRScanner icons
    'i-lucide-qr-code',
    'i-lucide-search',
    'i-lucide-share',
    'i-lucide-users',
    'i-lucide-x',
    'i-lucide-user',
    // VisibilityIcon icons
    'i-lucide-globe',
    'i-lucide-link',
    'i-lucide-lock',
    // Peer blocking icons
    'i-lucide-ban',
    'i-lucide-check',
    // Playlist control icons
    'i-lucide-shuffle',
    'i-lucide-repeat',
    'i-lucide-repeat-1',
    // Add to playlist modal icons
    'i-lucide-bookmark',
    'i-lucide-list-video',
    'i-lucide-video',
    'i-lucide-loader-2',
    'i-lucide-plus',
    'i-lucide-check-square',
    'i-lucide-square',
  ],
  presets: [
    presetUno(),
    presetIcons({
      scale: 1.2,
      extraProperties: {
        'display': 'inline-block',
        'vertical-align': 'middle',
      },
    }),
    presetTypography({
      cssExtend: {
        'p,li,td,th': {
          color: '#ffffff',
        },
        'h1,h2,h3,h4,h5,h6': {
          color: '#ffffff',
        },
        'a': {
          color: '#7647FE',
        },
        'code': {
          color: '#ffffff',
          background: '#272727',
          padding: '0.2em 0.4em',
          'border-radius': '4px',
        },
        'pre': {
          background: '#212121',
          'border-radius': '6px',
        },
        'pre code': {
          background: 'transparent',
          padding: '0',
        },
        'blockquote': {
          'border-left-color': '#3f3f3f',
          color: '#aaaaaa',
        },
        'hr': {
          'border-color': '#3f3f3f',
        },
        'table': {
          'border-color': '#3f3f3f',
        },
        'th,td': {
          'border-color': '#3f3f3f',
        },
        'strong': {
          color: '#ffffff',
        },
      },
    }),
  ],
  theme: {
    colors: {
      // YouTube dark theme colors
      surface: {
        0: '#0f0f0f',
        1: '#212121',
        2: '#272727',
        3: '#3f3f3f',
      },
      text: {
        1: '#ffffff',
        2: '#aaaaaa',
        3: '#606060',
      },
      accent: '#916dfe',
      success: '#2ba640',
      danger: '#ff0000',
      warning: '#ffcc00',
      // YouTube specific
      'yt-red': '#ff0000',
      'yt-desc-hover': '#392b07',
    },
    borderRadius: {
      DEFAULT: '6px',
      sm: '4px',
      lg: '8px',
    },
  },
  shortcuts: {
    // Layout
    'flex-center': 'flex items-center justify-center',
    'flex-between': 'flex items-center justify-between',

    // Card/Panel
    'card': 'bg-surface-1 rounded overflow-hidden',
    'card-header': 'p-3 b-b-1 b-b-solid b-b-surface-3 flex-between',

    // Buttons - YouTube style (rounded-full, no borders, quick transition)
    'btn': 'px-3 py-1.5 min-h-9 rounded-full text-sm font-medium transition-colors duration-100 select-none disabled:opacity-50 disabled:cursor-not-allowed',
    'btn-primary': 'btn bg-white text-black hover:bg-white/80 disabled:hover:bg-white',
    'btn-success': 'btn bg-success text-white hover:bg-success/80 disabled:hover:bg-success',
    'btn-danger': 'btn bg-danger text-white hover:bg-danger/80 disabled:hover:bg-danger',
    'btn-ghost': 'btn bg-surface-2 text-text-1 hover:bg-surface-3 disabled:hover:bg-surface-2',
    'btn-circle': 'w-9 min-h-9 p-0! rounded-full flex items-center justify-center transition-colors duration-100 select-none disabled:opacity-50 disabled:cursor-not-allowed',

    // Form - same height as buttons (py-1.5 to match btn), YouTube style with border
    'input': 'px-3 py-1.5 bg-surface-0 b-1 b-solid b-surface-3 rounded-full text-text-1 outline-none focus:b-accent',
    'textarea': 'px-3 py-2 bg-surface-0 b-1 b-solid b-surface-3 rounded text-text-1 outline-none focus:b-accent resize-y font-sans',

    // Text
    'text-muted': 'text-text-2',
    'text-subtle': 'text-text-3',

    // Live indicator
    'badge-live': 'bg-danger text-white text-xs px-1.5 py-0.5 rounded-sm font-semibold animate-pulse',

    // Pulse animation for recently changed files
    'animate-pulse-live': 'animate-pulse-bg',
  },
  rules: [
    // Custom pulse animation for file rows - gentle background glow
    ['animate-pulse-bg', {
      animation: 'pulse-bg 3s ease-in-out infinite',
    }],
    // Fade-in animation for delayed loading indicator
    ['animate-fade-in', {
      animation: 'fade-in 0.3s ease-in',
    }],
    // Hide scrollbar while keeping scroll functionality
    ['scrollbar-hide', {
      '-ms-overflow-style': 'none',
      'scrollbar-width': 'none',
    }],
  ],
  preflights: [
    {
      getCSS: () => `
        /* Reset button defaults */
        button {
          border: none;
          background: transparent;
          cursor: pointer;
          font: inherit;
          color: inherit;
        }
        @keyframes pulse-bg {
          0%, 100% { background-color: transparent; }
          50% { background-color: rgba(118, 71, 254, 0.08); }
        }
        @keyframes fade-in {
          0% { opacity: 0; }
          100% { opacity: 1; }
        }
        .scrollbar-hide::-webkit-scrollbar {
          display: none;
        }
      `,
    },
  ],
});
