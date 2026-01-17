/**
 * Tracks which app variant is currently running
 * Set by each main entry point (main.ts, main-video.ts, main-docs.ts, main-iris.ts, main-maps.ts)
 */
export type AppType = 'files' | 'video' | 'docs' | 'iris' | 'maps';

let currentAppType: AppType = 'files';

export function setAppType(type: AppType) {
  currentAppType = type;
}

export function getAppType(): AppType {
  return currentAppType;
}

export function isFilesApp(): boolean {
  return currentAppType === 'files';
}

export function isIrisApp(): boolean {
  return currentAppType === 'iris';
}

export function isMapsApp(): boolean {
  return currentAppType === 'maps';
}
