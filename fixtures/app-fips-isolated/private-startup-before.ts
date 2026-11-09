import { settingsStore } from './settings';
import { getConfiguredPrivateNostrRelays } from './htreeRuntime';
import { writable } from 'svelte/store';
import type { PrivateContactPatch, PrivateContactSyncState } from 'nostr-social-graph/privateContactSyncV2';
import type { PrivateContactSyncController, PrivateContactSyncStatus } from 'nostr-social-graph/privateContactSyncV2Controller';
import type { PrivateRatchetRuntime } from './privateRatchetRuntime';
import { accountsStore, type SocialAccount } from './accounts';
import { canSignAccount } from './accountSigner';
import { contactMemoryStore } from './contactMemoryStore';
import { waitForNextBrowserPaint } from './asyncTiming';
import { loadPrivateContactSync, loadPrivateContactSyncController } from './privateContactSyncModules';

type SyncView = { owner: string | null; status: PrivateContactSyncStatus; needsApproval: boolean;
  canAuthorize: boolean; link: string; error: string; anotherTab: boolean };
const emptyView: SyncView = { owner: null, status: 'local-only', needsApproval: false, canAuthorize: false, link: '', error: '', anotherTab: false };
export const privateContactSyncStatus = writable<SyncView>(emptyView);
// Keep the address so older tabs fail closed when they encounter the new version.
export const privateContactSyncKey = (owner: string): string => `iris-social:private-contacts:v1:${owner}`;
const enabledKey = (owner: string): string => `iris-social:private-ratchet-enabled:${owner}`;
const randomId = (): string => Array.from(crypto.getRandomValues(new Uint8Array(16)), byte => byte.toString(16).padStart(2, '0')).join('');
let active: ReturnType<typeof openAccount> | undefined;

function openAccount(account: SocialAccount) {
  const owner = account.pubkey, key = privateContactSyncKey(owner);
  const abort = new AbortController();
  let controller: PrivateContactSyncController | undefined;
  let runtime: PrivateRatchetRuntime | undefined;
  let ready: Promise<void> | undefined;
  let opening: Promise<void> | undefined;
  const savedEnabled = localStorage.getItem(enabledKey(owner));
  let enabled = savedEnabled === 'true'
    || (savedEnabled === null && account.method !== 'nip07' && canSignAccount(account));
  let registered = false, anotherTab = false, error = '', link = '', roster = '', storageFailed = false;
  let lastWake = 0;
  const view = (status = controller?.getStatus() ?? 'local-only'): void => {
    if (abort.signal.aborted) { return; }
    privateContactSyncStatus.set({ owner, status: storageFailed ? 'error' : registered ? status : 'local-only',
      needsApproval: !registered && !anotherTab, canAuthorize: canSignAccount(account), link, error, anotherTab });
  };
  view();
  const withLock = async (run: () => Promise<void>): Promise<void> => {
    if (!navigator.locks) { throw new Error('Update your browser to save private details safely.'); }
    await navigator.locks.request(key, run);
  };
  function project(state: PrivateContactSyncState): void {
    if (abort.signal.aborted || contactMemoryStore.getSnapshot().viewer !== owner) { return; }
    for (const [contact, fields] of Object.entries(state.contacts)) {
      const patch: PrivateContactPatch = {};
      if (fields.favorite) { patch.favorite = fields.favorite.value === true; }
      if (fields.nickname) { patch.nickname = typeof fields.nickname.value === 'string' ? fields.nickname.value : null; }
      if (fields.note) { patch.note = typeof fields.note.value === 'string' ? fields.note.value : null; }
      contactMemoryStore.applyPrivateFields(owner, contact, patch);
    }
  }
  async function initialize(): Promise<void> {
    await waitForNextBrowserPaint();
    if (abort.signal.aborted) { return; }
    const [{ createPrivateContactSync, migratePrivateContactSync }, { createPrivateContactSyncController }] = await Promise.all([
      loadPrivateContactSync(), loadPrivateContactSyncController(),
    ]);
    if (abort.signal.aborted) { return; }
    const load = (): PrivateContactSyncState | null => {
      const raw = localStorage.getItem(key);
      return raw ? migratePrivateContactSync(JSON.parse(raw), owner) : null;
    };
    let state: PrivateContactSyncState | undefined;
    await withLock(async () => {
      if (abort.signal.aborted) { return; }
      state = load() ?? createPrivateContactSync(owner, randomId());
      localStorage.setItem(key, JSON.stringify(state));
    });
    if (abort.signal.aborted) { return; }
    controller = createPrivateContactSyncController({
      state: state!, load, withLock,
      save: next => localStorage.setItem(key, JSON.stringify(next)),
      send: document => runtime && registered ? runtime.send(document) : Promise.resolve(false),
      onChange: next => project(next), onStatus: view,
      onError: () => { if (registered) { error = 'Waiting to sync. Your changes are saved.'; view('error'); } },
    });
    const legacy = contactMemoryStore.getSnapshot();
    if (legacy.viewer === owner) {
      for (const [contact, memory] of legacy.contacts) {
        await controller.seed(contact, { favorite: memory.favorite, nickname: memory.nickname, note: memory.note });
      }
    }
    storageFailed = false; project(controller.getState()); view();
  }
  function ensureReady(): Promise<void> {
    ready ??= initialize().catch(cause => { controller?.stop(); controller = undefined; ready = undefined; throw cause; });
    return ready;
  }
  async function catchUp(): Promise<void> {
    if (!runtime || !registered || !controller || abort.signal.aborted) { return; }
    await runtime.enable();
    await runtime.request();
    await controller.queueSnapshot();
    controller.retry();
    error = ''; link = ''; view();
  }
  /** Hold one ratchet writer per account; other tabs share only the contact registers. */
  async function openRuntime(): Promise<void> {
    if (runtime) { return; }
    if (opening) { return opening; }
    await ensureReady();
    if (abort.signal.aborted) { return; }
    opening = new Promise<void>((resolve, reject) => {
      void navigator.locks.request(`iris-social:private-ratchet:${owner}`, { ifAvailable: true }, async lock => {
        if (!lock || abort.signal.aborted) { anotherTab = !abort.signal.aborted; view(); resolve(); return; }
        anotherTab = false;
        const { createPrivateRatchetRuntime } = await import('./privateRatchetRuntime');
        runtime = await createPrivateRatchetRuntime({ account, signal: abort.signal,
          merge: document => controller!.mergeTrusted(document), snapshot: () => controller!.queueSnapshot(),
          onState: state => {
            registered = state.isCurrentDeviceRegistered;
            if (registered && !enabled) {
              enabled = true;
              try { localStorage.setItem(enabledKey(owner), 'true'); }
              catch { error = 'Could not remember this device. Please try again.'; }
            }
            const fingerprint = registered ? state.registeredDevices.map(device => device.identityPubkey).sort().join(':') : '';
            if (runtime && fingerprint && fingerprint !== roster) {
              roster = fingerprint;
              void catchUp().catch(cause => { error = cause instanceof Error ? cause.message : 'Waiting to sync.'; view(); });
            }
            view();
          },
          onError: () => { if (!abort.signal.aborted) { error = 'Waiting to sync. Your changes are saved.'; view(); } },
        });
        resolve();
        if (abort.signal.aborted) { runtime.stop(); return; }
        await new Promise<void>(release => abort.signal.addEventListener('abort', () => release(), { once: true }));
      }).catch(reject);
    }).finally(() => { opening = undefined; });
    return opening;
  }
  async function enable(): Promise<void> {
    await openRuntime();
    if (!runtime) { return; }
    enabled = true; error = '';
    localStorage.setItem(enabledKey(owner), 'true');
    try {
      await runtime.enable();
      abort.signal.throwIfAborted();
      localStorage.setItem(enabledKey(owner), 'true');
      registered = runtime.getState().isCurrentDeviceRegistered;
      roster = runtime.getState().registeredDevices.map(device => device.identityPubkey).sort().join(':');
      await catchUp();
    } catch (cause) {
      if (abort.signal.aborted) { return; }
      if (cause instanceof Error && cause.name === 'PrivateSyncApprovalError') {
        enabled = false; localStorage.setItem(enabledKey(owner), 'false');
      }
      error = cause instanceof Error ? cause.message : 'Could not link this device.';
      view();
    }
  }
  void ensureReady().then(() => enabled ? enable() : undefined).catch(cause => {
    storageFailed = !controller;
    error = storageFailed ? 'Could not open private storage. Your saved data has been kept.'
      : cause instanceof Error ? cause.message : 'Waiting to sync. Your changes are saved.';
    view('error');
  });
  return {
    owner,
    async edit(contact: string, patch: PrivateContactPatch): Promise<void> {
      await ensureReady();
      if (abort.signal.aborted || !controller) { throw new Error('The active account changed.'); }
      try { await controller.edit(contact, patch); storageFailed = false; }
      catch (cause) { storageFailed = true; error = 'Could not save your changes. Please try again.'; view('error'); throw cause; }
    },
    retry: enable,
    async link(): Promise<void> {
      await openRuntime();
      if (runtime) { link = await runtime.link(); view(); }
    },
    refresh: () => { void ensureReady().then(() => controller?.refresh()).catch(() => view('error')); },
    wake: () => {
      if (Date.now() - lastWake < 30_000) { return; }
      lastWake = Date.now();
      const savedEnabled = localStorage.getItem(enabledKey(owner));
      if (savedEnabled !== null) { enabled = savedEnabled === 'true'; }
      if (runtime) { void runtime.wake().then(() => controller?.retry()).catch(() => view()); }
      else if (enabled) { void enable().catch(() => view()); }
    },
    stop: () => { abort.abort(); controller?.stop(); runtime?.stop(); },
  };
}

export function retryPrivateContactSync(): void { void active?.retry().catch(() => undefined); }
export function linkPrivateContactDevice(): void { void active?.link().catch(() => undefined); }

export function startPrivateContactSync(): () => void {
  let identity = '';
  const syncAccount = (): void => {
    const state = accountsStore.getState();
    const account = state.accounts.find(value => value.pubkey === state.activeAccountPubkey);
    const nextIdentity = account ? `${account.pubkey}:${account.method}:${account.nsec ?? ''}:${JSON.stringify(getConfiguredPrivateNostrRelays())}` : '';
    if (!state.initialized || nextIdentity === identity) { return; }
    identity = nextIdentity;
    active?.stop(); active = undefined;
    contactMemoryStore.setViewer(account?.pubkey ?? null);
    if (account) { active = openAccount(account); }
    else { privateContactSyncStatus.set(emptyView); }
  };
  const unsubscribe = accountsStore.subscribe(syncAccount);
  const unsubscribeSettings = settingsStore.subscribe(syncAccount);
  contactMemoryStore.setPrivateEditHandler(async (owner, contact, patch) => {
    if (!active || active.owner !== owner) { throw new Error('Private contact storage is not ready.'); }
    await active.edit(contact, patch);
  });
  const onStorage = (event: StorageEvent): void => {
    if (!event.key || event.key === 'iris-social:settings') { syncAccount(); }
    if (active && (!event.key || event.key === privateContactSyncKey(active.owner))) { active.refresh(); } };
  const wake = (): void => { if (document.visibilityState !== 'hidden') { active?.wake(); } };
  const timer = setInterval(wake, 30_000);
  window.addEventListener('storage', onStorage); window.addEventListener('online', wake);
  document.addEventListener('visibilitychange', wake);
  return () => {
    unsubscribe(); unsubscribeSettings(); active?.stop(); active = undefined; contactMemoryStore.setPrivateEditHandler(null);
    clearInterval(timer); window.removeEventListener('storage', onStorage); window.removeEventListener('online', wake);
    document.removeEventListener('visibilitychange', wake);
  };
}
