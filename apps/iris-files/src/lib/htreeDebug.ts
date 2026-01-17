export interface HtreeDebugEntry {
  t: number;
  event: string;
  data?: Record<string, unknown>;
}

const DEBUG_STORAGE_KEY = 'htree.debug';
const DEBUG_QUERY_KEYS = ['htree_debug', 'htreeDebug', 'debug'];
const MAX_LOG_ENTRIES = 400;

type DebugConsole = Pick<Console, 'log' | 'info' | 'warn' | 'error' | 'debug'>;

interface DebugCaptureState {
  installed: boolean;
  originalConsole: DebugConsole;
}

function hasDebugQueryParam(): boolean {
  if (typeof window === 'undefined') return false;
  const checkParams = (params: URLSearchParams): boolean => {
    for (const key of DEBUG_QUERY_KEYS) {
      const value = params.get(key);
      if (value === '1' || value === 'true' || value === 'htree') {
        return true;
      }
    }
    return false;
  };

  try {
    if (checkParams(new URLSearchParams(window.location.search))) {
      return true;
    }
  } catch {}

  try {
    const hash = window.location.hash || '';
    const queryIndex = hash.indexOf('?');
    if (queryIndex !== -1) {
      const hashParams = new URLSearchParams(hash.slice(queryIndex + 1));
      if (checkParams(hashParams)) {
        return true;
      }
    }
  } catch {}

  return false;
}

declare global {
  interface Window {
    __HTREE_DEBUG__?: boolean;
    __HTREE_DEBUG_LOG__?: HtreeDebugEntry[];
    __HTREE_DEBUG_CAPTURE__?: DebugCaptureState;
  }
}

export function isHtreeDebugEnabled(): boolean {
  if (typeof window === 'undefined') return false;
  if (window.__HTREE_DEBUG__ === true) return true;
  if (hasDebugQueryParam()) return true;
  try {
    return localStorage.getItem(DEBUG_STORAGE_KEY) === '1';
  } catch {
    return false;
  }
}

function appendDebugEntry(entry: HtreeDebugEntry): void {
  if (typeof window === 'undefined') return;
  const log = window.__HTREE_DEBUG_LOG__ ?? [];
  log.push(entry);
  if (log.length > MAX_LOG_ENTRIES) {
    log.splice(0, log.length - MAX_LOG_ENTRIES);
  }
  window.__HTREE_DEBUG_LOG__ = log;
}

function formatConsoleArg(value: unknown): string {
  if (value instanceof Error) {
    return value.stack || `${value.name}: ${value.message}`;
  }
  if (typeof value === 'string') return value;
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

function shouldCaptureConsole(): boolean {
  if (typeof window === 'undefined') return false;
  if ((window as { __HTREE_DEBUG_VERBOSE__?: boolean }).__HTREE_DEBUG_VERBOSE__ === true) {
    return true;
  }
  try {
    return localStorage.getItem('htree.debug.console') === '1';
  } catch {
    return false;
  }
}

function getDebugConsole(): DebugConsole {
  if (typeof window !== 'undefined' && window.__HTREE_DEBUG_CAPTURE__?.originalConsole) {
    return window.__HTREE_DEBUG_CAPTURE__.originalConsole;
  }
  return console;
}

export function installHtreeDebugCapture(): void {
  if (typeof window === 'undefined') return;
  if (!isHtreeDebugEnabled()) return;
  if (window.__HTREE_DEBUG_CAPTURE__?.installed) return;

  const originalConsole: DebugConsole = {
    log: console.log.bind(console),
    info: console.info.bind(console),
    warn: console.warn.bind(console),
    error: console.error.bind(console),
    debug: console.debug.bind(console),
  };

  const capture = (level: keyof DebugConsole) => (...args: unknown[]) => {
    appendDebugEntry({
      t: Date.now(),
      event: `console:${level}`,
      data: { args: args.map(formatConsoleArg) },
    });
    originalConsole[level](...args);
  };

  const captureAll = shouldCaptureConsole();
  if (captureAll) {
    console.log = capture('log');
    console.info = capture('info');
    console.debug = capture('debug');
  }
  console.warn = capture('warn');
  console.error = capture('error');

  window.addEventListener('error', (event) => {
    appendDebugEntry({
      t: Date.now(),
      event: 'window:error',
      data: {
        message: event.message,
        filename: event.filename,
        lineno: event.lineno,
        colno: event.colno,
        stack: event.error instanceof Error ? event.error.stack : null,
      },
    });
  });

  window.addEventListener('unhandledrejection', (event) => {
    const reason = event.reason;
    appendDebugEntry({
      t: Date.now(),
      event: 'window:unhandledrejection',
      data: {
        reason: formatConsoleArg(reason),
      },
    });
  });

  window.__HTREE_DEBUG_CAPTURE__ = {
    installed: true,
    originalConsole,
  };
}

export function logHtreeDebug(event: string, data?: Record<string, unknown>): void {
  if (!isHtreeDebugEnabled()) return;
  installHtreeDebugCapture();

  const entry: HtreeDebugEntry = {
    t: Date.now(),
    event,
    data,
  };

  try {
    appendDebugEntry(entry);
  } catch {}

  const logger = getDebugConsole();
  if (data) {
    logger.log(`[htree] ${event}`, data);
  } else {
    logger.log(`[htree] ${event}`);
  }
}

if (typeof window !== 'undefined' && isHtreeDebugEnabled()) {
  installHtreeDebugCapture();
}
