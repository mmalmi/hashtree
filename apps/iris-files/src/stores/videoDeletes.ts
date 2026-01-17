const STORAGE_KEY = 'iris-video-deletes';

type DeleteMap = Record<string, number>;

let deletes: DeleteMap = {};

function loadDeletes(): void {
  if (typeof localStorage === 'undefined') return;
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) deletes = JSON.parse(raw) as DeleteMap;
  } catch {
    deletes = {};
  }
}

function persistDeletes(): void {
  if (typeof localStorage === 'undefined') return;
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(deletes));
  } catch {
    // Ignore storage errors
  }
}

loadDeletes();

function keyFor(ownerNpub: string, treeName: string): string {
  return `${ownerNpub}/${treeName}`;
}

export function recordDeletedVideo(ownerNpub: string, treeName: string, deletedAt: number): void {
  deletes[keyFor(ownerNpub, treeName)] = deletedAt;
  persistDeletes();
}

export function getDeletedVideoTimestamp(ownerNpub: string, treeName: string): number | undefined {
  return deletes[keyFor(ownerNpub, treeName)];
}

export function clearDeletedVideo(ownerNpub: string, treeName: string): void {
  const key = keyFor(ownerNpub, treeName);
  if (!(key in deletes)) return;
  delete deletes[key];
  persistDeletes();
}
