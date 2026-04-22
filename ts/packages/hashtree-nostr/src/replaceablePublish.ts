import {
  isParameterizedReplaceableKind,
  isReplaceableKind,
  parameterizedReplaceableKey,
  replaceableKey,
} from './eventKeys.js';

export interface ReplaceableEventCoordinate {
  pubkey: string;
  kind: number;
  dTag?: string;
}

export interface ReplaceableEventTemplateLike {
  kind: number;
  tags: string[][];
}

export interface ReplaceablePublishQueueConfig {
  nowMs?: () => number;
  sleep?: (ms: number) => Promise<void>;
}

export interface ReplaceablePublishRequest<TResult> {
  coordinate: string | ReplaceableEventCoordinate;
  publish: (createdAt: number) => Promise<TResult>;
}

export type ReplaceablePublishOutcome<TResult> =
  | { status: 'published'; createdAt: number; result: TResult }
  | { status: 'superseded' };

interface QueueEntry<TResult> {
  publish: (createdAt: number) => Promise<TResult>;
  resolve: (value: ReplaceablePublishOutcome<TResult>) => void;
  reject: (reason?: unknown) => void;
}

interface PublishSlot {
  active: boolean;
  lastCreatedAt: number | null;
  lastTouchedAtMs: number;
  queued: QueueEntry<any> | null;
}

const IDLE_SLOT_RETENTION_MS = 60_000;

function sleepFor(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}

function normalizeTagValue(value: unknown): string {
  return typeof value === 'string' ? value : '';
}

function getDTag(tags: readonly string[][]): string | null {
  for (const tag of tags) {
    if (tag[0] !== 'd') {
      continue;
    }
    const value = normalizeTagValue(tag[1]).trim();
    if (value) {
      return value;
    }
  }

  return null;
}

export function replaceableEventCoordinateKey(coordinate: ReplaceableEventCoordinate): string {
  if (isReplaceableKind(coordinate.kind)) {
    return replaceableKey(coordinate.pubkey, coordinate.kind);
  }

  if (isParameterizedReplaceableKind(coordinate.kind)) {
    const dTag = coordinate.dTag?.trim();
    if (!dTag) {
      throw new Error('Parameterized replaceable coordinates require a non-empty d tag');
    }
    return parameterizedReplaceableKey(coordinate.pubkey, coordinate.kind, dTag);
  }

  throw new Error(`Kind ${coordinate.kind} is not replaceable`);
}

export function replaceableEventCoordinateFromTemplate(
  pubkey: string,
  event: ReplaceableEventTemplateLike,
): ReplaceableEventCoordinate {
  if (isReplaceableKind(event.kind)) {
    return { pubkey, kind: event.kind };
  }

  if (isParameterizedReplaceableKind(event.kind)) {
    const dTag = getDTag(event.tags);
    if (!dTag) {
      throw new Error('Parameterized replaceable events require a non-empty d tag');
    }
    return { pubkey, kind: event.kind, dTag };
  }

  throw new Error(`Kind ${event.kind} is not replaceable`);
}

async function waitForSlot(options: {
  nowMs: () => number;
  sleep: (ms: number) => Promise<void>;
  slot: PublishSlot;
}): Promise<number> {
  const { nowMs, sleep, slot } = options;
  while (slot.lastCreatedAt !== null && Math.floor(nowMs() / 1_000) <= slot.lastCreatedAt) {
    const earliestNextDispatchMs = (slot.lastCreatedAt + 1) * 1_000;
    await sleep(Math.max(1, earliestNextDispatchMs - nowMs()));
  }

  const dispatchAtMs = nowMs();
  const createdAt = Math.floor(dispatchAtMs / 1_000);
  slot.lastCreatedAt = createdAt;
  return createdAt;
}

export function createReplaceablePublishQueue(config: ReplaceablePublishQueueConfig = {}) {
  const nowMs = config.nowMs ?? (() => Date.now());
  const sleep = config.sleep ?? sleepFor;
  const slots = new Map<string, PublishSlot>();

  function pruneIdleSlots(): void {
    const now = nowMs();
    for (const [key, slot] of slots) {
      if (slot.active || slot.queued) {
        continue;
      }
      if (now - slot.lastTouchedAtMs > IDLE_SLOT_RETENTION_MS) {
        slots.delete(key);
      }
    }
  }

  async function drain(key: string, slot: PublishSlot): Promise<void> {
    try {
      while (slot.queued) {
        const entry = slot.queued;
        slot.queued = null;

        const createdAt = await waitForSlot({ nowMs, sleep, slot });
        slot.lastTouchedAtMs = nowMs();
        try {
          const result = await entry.publish(createdAt);
          entry.resolve({ status: 'published', createdAt, result });
        } catch (error) {
          entry.reject(error);
        }
      }
    } finally {
      slot.active = false;
      if (slot.queued) {
        slot.active = true;
        void drain(key, slot);
        return;
      }
      slot.lastTouchedAtMs = nowMs();
    }
  }

  return {
    publish<TResult>(request: ReplaceablePublishRequest<TResult>): Promise<ReplaceablePublishOutcome<TResult>> {
      pruneIdleSlots();
      const key = typeof request.coordinate === 'string'
        ? request.coordinate
        : replaceableEventCoordinateKey(request.coordinate);
      let slot = slots.get(key);
      if (!slot) {
        slot = {
          active: false,
          lastCreatedAt: null,
          lastTouchedAtMs: nowMs(),
          queued: null,
        };
        slots.set(key, slot);
      }

      return new Promise<ReplaceablePublishOutcome<TResult>>((resolve, reject) => {
        const queuedEntry = slot!.queued as QueueEntry<TResult> | null;
        if (queuedEntry) {
          queuedEntry.resolve({ status: 'superseded' });
        }

        slot!.lastTouchedAtMs = nowMs();
        slot!.queued = {
          publish: request.publish,
          resolve,
          reject,
        };

        if (!slot!.active) {
          slot!.active = true;
          void drain(key, slot!);
        }
      });
    },
  };
}
