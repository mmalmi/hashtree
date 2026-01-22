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
    base += '/';

    if (typeof window !== 'undefined' && window.location?.origin) {
      return new URL(base, window.location.origin).toString();
    }
    return base;
  });

  let iframeSrc = $state<string>('');

  const SANDBOX_SHIM_SCRIPT = [
    '(function () {',
    "  'use strict';",
    '  function schedule(fn) {',
    "    if (typeof queueMicrotask === 'function') {",
    '      queueMicrotask(fn);',
    '    } else {',
    '      Promise.resolve().then(fn);',
    '    }',
    '  }',
    '  function canUseStorage(name) {',
    '    try {',
    '      const storage = window[name];',
    '      if (!storage) return false;',
    "      const key = '__iris_storage_test__';",
    "      storage.setItem(key, '1');",
    '      storage.removeItem(key);',
    '      return true;',
    '    } catch (err) {',
    '      return false;',
    '    }',
    '  }',
    '  function createStorage() {',
    '    const data = new Map();',
    '    return {',
    '      get length() { return data.size; },',
    '      key: function (index) {',
    '        if (index < 0 || index >= data.size) return null;',
    '        let i = 0;',
    '        for (const key of data.keys()) {',
    '          if (i === index) return key;',
    '          i += 1;',
    '        }',
    '        return null;',
    '      },',
    '      getItem: function (key) {',
    '        const value = data.get(String(key));',
    '        return value === undefined ? null : value;',
    '      },',
    '      setItem: function (key, value) {',
    '        data.set(String(key), String(value));',
    '      },',
    '      removeItem: function (key) {',
    '        data.delete(String(key));',
    '      },',
    '      clear: function () { data.clear(); }',
    '    };',
    '  }',
    '  function defineGlobal(name, value) {',
    '    try { Object.defineProperty(window, name, { value: value, configurable: true, writable: true }); return; } catch (err) {}',
    '    try { window[name] = value; return; } catch (err) {}',
    '    try { Object.defineProperty(globalThis, name, { value: value, configurable: true, writable: true }); return; } catch (err) {}',
    '    try { globalThis[name] = value; } catch (err) {}',
    '  }',
    '  function ensureStorage(name) {',
    '    if (canUseStorage(name)) return;',
    '    const storage = createStorage();',
    '    defineGlobal(name, storage);',
    '  }',
    "  ensureStorage('localStorage');",
    "  ensureStorage('sessionStorage');",
    '  function isOpaqueOrigin() {',
    "    try { return window.location.origin === 'null'; } catch (err) { return true; }",
    '  }',
    '  function ensureServiceWorkerStub() {',
    '    try { if (navigator.serviceWorker) return; } catch (err) {}',
    '    const stub = {',
    '      register: function () { return Promise.resolve(stub); },',
    '      getRegistration: function () { return Promise.resolve(stub); },',
    '      getRegistrations: function () { return Promise.resolve([stub]); },',
    '      controller: null,',
    '      update: function () { return Promise.resolve(); },',
    '      unregister: function () { return Promise.resolve(true); }',
    '    };',
    '    stub.ready = Promise.resolve(stub);',
    "    try { Object.defineProperty(navigator, 'serviceWorker', { value: stub, configurable: true }); } catch (err) {}",
    '  }',
    '  ensureServiceWorkerStub();',
    '  let nativeIndexedDb = null;',
    '  try { nativeIndexedDb = window.indexedDB; } catch (err) {}',
    '  function canUseIndexedDB() {',
    '    try {',
    '      if (!nativeIndexedDb) return false;',
    "      const request = nativeIndexedDb.open('__iris_probe__', 1);",
    '      if (!request) return false;',
    '      request.onerror = function () {};',
    '      request.onsuccess = function () {',
    '        try { request.result.close(); } catch (err) {}',
    "        try { nativeIndexedDb.deleteDatabase('__iris_probe__'); } catch (err) {}",
    '      };',
    '      return true;',
    '    } catch (err) {',
    '      return false;',
    '    }',
    '  }',
    '  if (nativeIndexedDb && canUseIndexedDB()) {',
    "    defineGlobal('__irisIndexedDB', nativeIndexedDb);",
    '    return;',
    '  }',
    '  const dbs = new Map();',
    '  function compareKeys(a, b) {',
    "    if (typeof a === 'number' && typeof b === 'number') {",
    '      return a - b;',
    '    }',
    '    const aStr = String(a);',
    '    const bStr = String(b);',
    '    if (aStr < bStr) return -1;',
    '    if (aStr > bStr) return 1;',
    '    return 0;',
    '  }',
    '  function getKeyFromPath(value, keyPath) {',
    '    if (!keyPath) return undefined;',
    '    if (Array.isArray(keyPath)) {',
    '      return keyPath.map(function (path) { return getKeyFromPath(value, path); });',
    '    }',
    "    if (typeof keyPath !== 'string') return undefined;",
    "    const parts = keyPath.split('.');",
    '    let current = value;',
    '    for (const part of parts) {',
    "      if (!current || typeof current !== 'object') return undefined;",
    '      current = current[part];',
    '    }',
    '    return current;',
    '  }',
    '  function inRange(range, key) {',
    '    if (!range) return true;',
    "    if (typeof range.includes === 'function') {",
    '      return range.includes(key);',
    '    }',
    '    if (range.lower !== undefined) {',
    '      const cmp = compareKeys(key, range.lower);',
    '      if (cmp < 0 || (cmp === 0 && range.lowerOpen)) return false;',
    '    }',
    '    if (range.upper !== undefined) {',
    '      const cmp = compareKeys(key, range.upper);',
    '      if (cmp > 0 || (cmp === 0 && range.upperOpen)) return false;',
    '    }',
    '    return true;',
    '  }',
    '  function IrisKeyRange(lower, upper, lowerOpen, upperOpen) {',
    '    this.lower = lower;',
    '    this.upper = upper;',
    '    this.lowerOpen = !!lowerOpen;',
    '    this.upperOpen = !!upperOpen;',
    '  }',
    '  IrisKeyRange.prototype.includes = function (value) {',
    '    return inRange(this, value);',
    '  };',
    '  IrisKeyRange.only = function (value) {',
    '    return new IrisKeyRange(value, value, false, false);',
    '  };',
    '  IrisKeyRange.lowerBound = function (lower, open) {',
    '    return new IrisKeyRange(lower, undefined, !!open, false);',
    '  };',
    '  IrisKeyRange.upperBound = function (upper, open) {',
    '    return new IrisKeyRange(undefined, upper, false, !!open);',
    '  };',
    '  IrisKeyRange.bound = function (lower, upper, lowerOpen, upperOpen) {',
    '    return new IrisKeyRange(lower, upper, !!lowerOpen, !!upperOpen);',
    '  };',
    '  function IrisRequest() {',
    '    this.result = undefined;',
    '    this.error = null;',
    '    this.onsuccess = null;',
    '    this.onerror = null;',
    '  }',
    '  IrisRequest.prototype._success = function (result) {',
    '    this.result = result;',
    '    const self = this;',
    '    schedule(function () {',
    "      if (typeof self.onsuccess === 'function') {",
    '        self.onsuccess({ target: self });',
    '      }',
    '    });',
    '  };',
    '  IrisRequest.prototype._error = function (error) {',
    '    this.error = error;',
    '    const self = this;',
    '    schedule(function () {',
    "      if (typeof self.onerror === 'function') {",
    '        self.onerror({ target: self });',
    '      }',
    '    });',
    '  };',
    '  function IrisOpenRequest() {',
    '    IrisRequest.call(this);',
    '    this.onupgradeneeded = null;',
    '    this.onblocked = null;',
    '    this.transaction = null;',
    '  }',
    '  IrisOpenRequest.prototype = Object.create(IrisRequest.prototype);',
    '  IrisOpenRequest.prototype.constructor = IrisOpenRequest;',
    '  function IrisStoreNames(stores) {',
    '    this._stores = stores;',
    '  }',
    '  IrisStoreNames.prototype.contains = function (name) {',
    '    return this._stores.has(name);',
    '  };',
    '  function IrisDatabase(name, version) {',
    '    this.name = name;',
    '    this.version = version;',
    '    this._stores = new Map();',
    '  }',
    "  Object.defineProperty(IrisDatabase.prototype, 'objectStoreNames', {",
    '    get: function () {',
    '      return new IrisStoreNames(this._stores);',
    '    }',
    '  });',
    '  IrisDatabase.prototype.createObjectStore = function (name, options) {',
    "    if (this._stores.has(name)) { throw new Error('ConstraintError'); }",
    '    const store = new IrisObjectStore(name, options);',
    '    this._stores.set(name, store);',
    '    return store;',
    '  };',
    '  IrisDatabase.prototype.deleteObjectStore = function (name) {',
    '    this._stores.delete(name);',
    '  };',
    '  IrisDatabase.prototype.transaction = function (storeNames, mode) {',
    '    return new IrisTransaction(this, storeNames, mode);',
    '  };',
    '  IrisDatabase.prototype.close = function () {};',
    '  function IrisTransaction(db, storeNames, mode) {',
    '    this.db = db;',
    "    this.mode = mode || 'readonly';",
    '    this._storeNames = Array.isArray(storeNames) ? storeNames : [storeNames];',
    '  }',
    '  IrisTransaction.prototype.objectStore = function (name) {',
    '    return this.db._stores.get(name);',
    '  };',
    '  IrisTransaction.prototype.commit = function () {};',
    '  function IrisObjectStore(name, options) {',
    '    this.name = name;',
    '    this.keyPath = options && options.keyPath ? options.keyPath : null;',
    '    this._records = new Map();',
    '    this._indexes = new Map();',
    '  }',
    '  IrisObjectStore.prototype.put = function (value, key) {',
    '    const request = new IrisRequest();',
    '    const store = this;',
    '    schedule(function () {',
    '      let resolvedKey = key;',
    '      if (resolvedKey === undefined && store.keyPath) {',
    '        resolvedKey = getKeyFromPath(value, store.keyPath);',
    '      }',
    '      if (resolvedKey === undefined || resolvedKey === null) {',
    "        request._error(new Error('DataError'));",
    '        return;',
    '      }',
    '      store._records.set(resolvedKey, value);',
    '      request._success(resolvedKey);',
    '    });',
    '    return request;',
    '  };',
    '  IrisObjectStore.prototype.get = function (key) {',
    '    const request = new IrisRequest();',
    '    const store = this;',
    '    schedule(function () {',
    '      request._success(store._records.get(key));',
    '    });',
    '    return request;',
    '  };',
    '  IrisObjectStore.prototype.delete = function (key) {',
    '    const request = new IrisRequest();',
    '    const store = this;',
    '    schedule(function () {',
    '      store._records.delete(key);',
    '      request._success(undefined);',
    '    });',
    '    return request;',
    '  };',
    '  IrisObjectStore.prototype.createIndex = function (name, keyPath) {',
    '    const index = new IrisIndex(this, name, keyPath);',
    '    this._indexes.set(name, index);',
    '    return index;',
    '  };',
    '  IrisObjectStore.prototype.index = function (name) {',
    '    return this._indexes.get(name);',
    '  };',
    '  IrisObjectStore.prototype.openCursor = function (range, direction) {',
    '    const entries = [];',
    '    this._records.forEach(function (value, key) {',
    '      if (inRange(range, key)) {',
    '        entries.push({ key: key, primaryKey: key, value: value });',
    '      }',
    '    });',
    '    entries.sort(function (a, b) { return compareKeys(a.key, b.key); });',
    "    if (direction === 'prev') entries.reverse();",
    '    return openCursorWithEntries(entries, this);',
    '  };',
    '  function IrisIndex(store, name, keyPath) {',
    '    this.objectStore = store;',
    '    this.name = name;',
    '    this.keyPath = keyPath;',
    '  }',
    '  IrisIndex.prototype.openCursor = function (range, direction) {',
    '    const entries = [];',
    '    this.objectStore._records.forEach((value, primaryKey) => {',
    '      const indexKey = getKeyFromPath(value, this.keyPath);',
    '      if (indexKey === undefined || indexKey === null) return;',
    '      if (inRange(range, indexKey)) {',
    '        entries.push({ key: indexKey, primaryKey: primaryKey, value: value });',
    '      }',
    '    });',
    '    entries.sort(function (a, b) { return compareKeys(a.key, b.key); });',
    "    if (direction === 'prev') entries.reverse();",
    '    return openCursorWithEntries(entries, this.objectStore);',
    '  };',
    '  function IrisCursor(entries, store, request) {',
    '    this._entries = entries;',
    '    this._index = 0;',
    '    this._store = store;',
    '    this._request = request;',
    '  }',
    "  Object.defineProperty(IrisCursor.prototype, 'value', {",
    '    get: function () {',
    '      return this._entries[this._index].value;',
    '    }',
    '  });',
    "  Object.defineProperty(IrisCursor.prototype, 'key', {",
    '    get: function () {',
    '      return this._entries[this._index].key;',
    '    }',
    '  });',
    "  Object.defineProperty(IrisCursor.prototype, 'primaryKey', {",
    '    get: function () {',
    '      return this._entries[this._index].primaryKey;',
    '    }',
    '  });',
    '  IrisCursor.prototype.continue = function () {',
    '    this._index += 1;',
    '    if (this._index >= this._entries.length) {',
    '      this._request._success(null);',
    '    } else {',
    '      this._request._success(this);',
    '    }',
    '  };',
    '  IrisCursor.prototype.delete = function () {',
    '    const entry = this._entries[this._index];',
    '    if (entry) {',
    '      this._store._records.delete(entry.primaryKey);',
    '    }',
    '  };',
    '  function openCursorWithEntries(entries, store) {',
    '    const request = new IrisRequest();',
    '    schedule(function () {',
    '      if (!entries.length) {',
    '        request._success(null);',
    '        return;',
    '      }',
    '      const cursor = new IrisCursor(entries, store, request);',
    '      request._success(cursor);',
    '    });',
    '    return request;',
    '  }',
    '  function openDatabase(name, version) {',
    '    const request = new IrisOpenRequest();',
    '    schedule(function () {',
    '      let db = dbs.get(name);',
    '      let needsUpgrade = false;',
    '      const desiredVersion = typeof version === \'number\' ? version : 1;',
    '      if (!db) {',
    '        db = new IrisDatabase(name, desiredVersion);',
    '        dbs.set(name, db);',
    '        needsUpgrade = true;',
    '      } else if (typeof version === \'number\') {',
    '        if (version < db.version) {',
    "          request._error(new Error('VersionError'));",
    '          return;',
    '        }',
    '        if (version > db.version) {',
    '          db.version = version;',
    '          needsUpgrade = true;',
    '        }',
    '      }',
    '      request.result = db;',
    '      if (needsUpgrade && typeof request.onupgradeneeded === \'function\') {',
    '        request.onupgradeneeded({ target: request });',
    '      }',
    '      request._success(db);',
    '    });',
    '    return request;',
    '  }',
    '  function deleteDatabase(name) {',
    '    const request = new IrisRequest();',
    '    schedule(function () {',
    '      dbs.delete(name);',
    '      request._success(undefined);',
    '    });',
    '    return request;',
    '  }',
    "  defineGlobal('IDBKeyRange', IrisKeyRange);",
    '  const polyfillFactory = {',
    '    open: openDatabase,',
    '    deleteDatabase: deleteDatabase',
    '  };',
    "  defineGlobal('indexedDB', polyfillFactory);",
    "  defineGlobal('__irisIndexedDB', polyfillFactory);",
    '  try {',
    '    if (window.indexedDB && window.indexedDB !== polyfillFactory) {',
    '      try { window.indexedDB.open = openDatabase; } catch (err) {}',
    '      try { Object.defineProperty(window.indexedDB, \'open\', { value: openDatabase, configurable: true }); } catch (err) {}',
    '      try { window.indexedDB.deleteDatabase = deleteDatabase; } catch (err) {}',
    '      try {',
    '        Object.defineProperty(window.indexedDB, \'deleteDatabase\', { value: deleteDatabase, configurable: true });',
    '      } catch (err) {}',
    '      try {',
    '        const proto = Object.getPrototypeOf(window.indexedDB);',
    '        if (proto) {',
    '          try { proto.open = openDatabase; } catch (err) {}',
    '          try { Object.defineProperty(proto, \'open\', { value: openDatabase, configurable: true }); } catch (err) {}',
    '          try { proto.deleteDatabase = deleteDatabase; } catch (err) {}',
    '          try {',
    '            Object.defineProperty(proto, \'deleteDatabase\', { value: deleteDatabase, configurable: true });',
    '          } catch (err) {}',
    '        }',
    '      } catch (err) {}',
    '    }',
    '  } catch (err) {}',
    '})();'
  ].join('\n');

  function injectSandboxShims(doc: Document, head: HTMLElement, baseEl: HTMLBaseElement | null) {
    if (head.querySelector('script[data-iris-sandbox-shims]')) {
      return;
    }
    const script = doc.createElement('script');
    script.setAttribute('data-iris-sandbox-shims', 'true');
    script.textContent = SANDBOX_SHIM_SCRIPT;
    if (baseEl && baseEl.parentNode === head) {
      head.insertBefore(script, baseEl.nextSibling);
    } else {
      head.prepend(script);
    }
  }

  function isRelativeResource(href: string | null): href is string {
    if (!href) return false;
    if (href.startsWith('//')) return false;
    if (href.startsWith('data:') || href.startsWith('blob:')) return false;
    return !/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(href);
  }

  function normalizeRelativePath(href: string): string[] {
    const cleanHref = href.split(/[?#]/)[0];
    const parts = cleanHref.split('/').filter(part => part.length > 0);
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

  function rewriteRootRelativeUrl(value: string | null): string | null {
    if (!value) return null;
    if (value.startsWith('//')) return value;
    if (value.startsWith('data:') || value.startsWith('blob:')) return value;
    if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(value)) return value;
    if (value.startsWith('/htree/')) return value;
    if (value.startsWith('/')) return value.slice(1);
    return value;
  }

  function rewriteRootRelativeSrcset(value: string | null): string | null {
    if (!value) return null;
    const entries = value.split(',').map(entry => entry.trim()).filter(Boolean);
    return entries.map((entry) => {
      const parts = entry.split(/\s+/);
      const url = parts.shift() ?? '';
      const updated = rewriteRootRelativeUrl(url) ?? url;
      return [updated, ...parts].join(' ');
    }).join(', ');
  }

  function rewriteRootRelativeAttributes(doc: Document) {
    for (const script of Array.from(doc.querySelectorAll('script[src]'))) {
      const updated = rewriteRootRelativeUrl(script.getAttribute('src'));
      if (updated && updated !== script.getAttribute('src')) {
        script.setAttribute('src', updated);
      }
    }

    const linkRelAllowlist = new Set([
      'stylesheet',
      'icon',
      'shortcut',
      'apple-touch-icon',
      'manifest',
      'preload',
      'modulepreload',
    ]);
    for (const link of Array.from(doc.querySelectorAll('link[href]'))) {
      const rel = (link.getAttribute('rel') || '').toLowerCase();
      const tokens = rel.split(/\s+/).filter(Boolean);
      if (!tokens.some(token => linkRelAllowlist.has(token))) continue;
      const updated = rewriteRootRelativeUrl(link.getAttribute('href'));
      if (updated && updated !== link.getAttribute('href')) {
        link.setAttribute('href', updated);
      }
    }

    for (const img of Array.from(doc.querySelectorAll('img[src]'))) {
      const updated = rewriteRootRelativeUrl(img.getAttribute('src'));
      if (updated && updated !== img.getAttribute('src')) {
        img.setAttribute('src', updated);
      }
    }

    for (const source of Array.from(doc.querySelectorAll('source[src]'))) {
      const updated = rewriteRootRelativeUrl(source.getAttribute('src'));
      if (updated && updated !== source.getAttribute('src')) {
        source.setAttribute('src', updated);
      }
    }

    for (const media of Array.from(doc.querySelectorAll('video[poster]'))) {
      const updated = rewriteRootRelativeUrl(media.getAttribute('poster'));
      if (updated && updated !== media.getAttribute('poster')) {
        media.setAttribute('poster', updated);
      }
    }

    for (const element of Array.from(doc.querySelectorAll('img[srcset], source[srcset]'))) {
      const updated = rewriteRootRelativeSrcset(element.getAttribute('srcset'));
      if (updated && updated !== element.getAttribute('srcset')) {
        element.setAttribute('srcset', updated);
      }
    }
  }

  function splitUrlPath(value: string): { path: string; suffix: string } {
    const match = value.match(/^[^?#]+/);
    const path = match ? match[0] : '';
    const suffix = value.slice(path.length);
    return { path, suffix };
  }

  function resolveRelativePath(baseParts: string[], relative: string): string[] {
    const parts = relative.split('/').filter(part => part.length > 0);
    const stack = [...baseParts];
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

  function guessMimeTypeFromPath(path: string): string {
    const lower = path.toLowerCase();
    if (lower.endsWith('.png')) return 'image/png';
    if (lower.endsWith('.jpg') || lower.endsWith('.jpeg')) return 'image/jpeg';
    if (lower.endsWith('.gif')) return 'image/gif';
    if (lower.endsWith('.webp')) return 'image/webp';
    if (lower.endsWith('.svg')) return 'image/svg+xml';
    if (lower.endsWith('.ico')) return 'image/x-icon';
    if (lower.endsWith('.avif')) return 'image/avif';
    if (lower.endsWith('.woff2')) return 'font/woff2';
    if (lower.endsWith('.woff')) return 'font/woff';
    if (lower.endsWith('.ttf')) return 'font/ttf';
    if (lower.endsWith('.otf')) return 'font/otf';
    return 'application/octet-stream';
  }

  function bytesToBase64(data: Uint8Array): string {
    let binary = '';
    for (let i = 0; i < data.length; i++) {
      binary += String.fromCharCode(data[i]);
    }
    return btoa(binary);
  }

  function patchIndexedDbReferences(scriptText: string): string {
    if (!scriptText.includes('window.indexedDB')) {
      return scriptText;
    }
    return scriptText.replace(/\bwindow\.indexedDB\b/g, 'window.__irisIndexedDB');
  }

  async function inlineCssUrls(
    cssText: string,
    cssPath: string,
    dirCid: typeof currentDirCid,
    tree: ReturnType<typeof getTree>
  ): Promise<string> {
    const matches = Array.from(cssText.matchAll(/url\((['"]?)([^'")]+)\1\)/gi));
    if (matches.length === 0 || !dirCid) return cssText;

    const baseParts = cssPath.split('/').filter(Boolean).slice(0, -1);
    let result = '';
    let lastIndex = 0;

    for (const match of matches) {
      const full = match[0];
      const quote = match[1] ?? '';
      const rawUrl = match[2] ?? '';
      const index = match.index ?? 0;
      result += cssText.slice(lastIndex, index);
      lastIndex = index + full.length;

      const url = rawUrl.trim();
      if (!url || url.startsWith('#') || url.startsWith('data:') || url.startsWith('blob:') || url.startsWith('//') || /^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(url)) {
        result += full;
        continue;
      }

      const { path, suffix } = splitUrlPath(url);
      if (!path || path.startsWith('/htree/')) {
        result += full;
        continue;
      }

      let resolvedParts: string[] = [];
      if (path.startsWith('/')) {
        resolvedParts = normalizeRelativePath(path);
      } else {
        resolvedParts = resolveRelativePath(baseParts, path);
      }

      if (resolvedParts.length === 0) {
        result += full;
        continue;
      }

      try {
        const resolved = await tree.resolvePath(dirCid, resolvedParts);
        if (!resolved) {
          const fallback = path.startsWith('/') ? path.slice(1) : resolvedParts.join('/');
          result += `url(${quote}${fallback}${suffix}${quote})`;
          continue;
        }
        const data = await tree.readFile(resolved.cid);
        if (!data) {
          result += full;
          continue;
        }
        const mimeType = guessMimeTypeFromPath(resolvedParts[resolvedParts.length - 1] || path);
        const base64 = bytesToBase64(data);
        result += `url(${quote}data:${mimeType};base64,${base64}${quote})`;
      } catch {
        result += full;
      }
    }

    result += cssText.slice(lastIndex);
    return result;
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
    let baseEl: HTMLBaseElement | null = null;
    if (existingBase) {
      existingBase.setAttribute('href', baseHref);
      baseEl = existingBase;
    } else {
      baseEl = doc.createElement('base');
      baseEl.setAttribute('href', baseHref);
      head.prepend(baseEl);
    }

    injectSandboxShims(doc, head, baseEl);
    rewriteRootRelativeAttributes(doc);

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
        const cssPath = normalizeRelativePath(href).join('/');
        const cssText = decoder.decode(data);
        styleEl.textContent = await inlineCssUrls(cssText, cssPath, dirCid, tree);
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
        const scriptText = decoder.decode(data);
        inlineScript.textContent = patchIndexedDbReferences(scriptText);
        script.replaceWith(inlineScript);
      } catch {
        // Keep original script if inlining fails
      }
    }

    const images = Array.from(doc.querySelectorAll('img[src], source[src]'));
    for (const img of images) {
      const src = img.getAttribute('src');
      if (!isRelativeResource(src)) continue;
      const parts = normalizeRelativePath(src);
      if (parts.length === 0) continue;
      try {
        const resolved = await tree.resolvePath(dirCid, parts);
        if (!resolved) continue;
        const data = await tree.readFile(resolved.cid);
        if (!data) continue;
        const mimeType = guessMimeTypeFromPath(parts[parts.length - 1] || src || '');
        const base64 = bytesToBase64(data);
        img.setAttribute('src', `data:${mimeType};base64,${base64}`);
      } catch {
        // Keep original src if inlining fails
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
      sandbox="allow-scripts allow-forms"
      title={fileName}
    ></iframe>
  {:else}
    <div class="flex-1 flex items-center justify-center text-muted">
      Loading...
    </div>
  {/if}
</div>
